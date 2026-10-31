//! Outbound webhooks: one https URL per team, signed with HMAC-SHA256 (see
//! `socialreins_core::webhook`). A target's status change queues its event in
//! `webhook_deliveries` in the same statement (`with_event`), once per target and event; a tick
//! every minute sends due rows, retrying with backoff. The URL must be public: names resolve
//! through `PublicOnly` and reqwest connects to exactly the checked addresses, with redirects
//! off.
use std::{net::IpAddr, sync::Arc, time::Duration};

use chrono::Utc;
use reqwest::{header::CONTENT_TYPE, Url};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement, TransactionTrait, Value};
use socialreins_core::{token, webhook};
use uuid::Uuid;

use crate::{
    connectors::bluesky::{public, PublicOnly, NOT_PUBLIC},
    state::AppState,
};

pub const MAX_URL_CHARS: usize = 2000;
/// Characters of random secret after `whsec_`.
const SECRET_CHARS: usize = 48;
/// Wait before the next try, by tries made so far; after the last, it gives up.
const BACKOFF_MINUTES: [i32; 7] = [1, 5, 30, 120, 360, 720, 1440];
pub const MAX_ATTEMPTS: i32 = BACKOFF_MINUTES.len() as i32 + 1;
/// Deliveries sent per tick, all at once; the rest wait for the next one.
const BATCH: i64 = 50;
const DELIVERY_LOCK: i64 = 0x0057_4542_484f_4f4b;

/// The HTTP client for webhooks.
#[derive(Clone)]
pub struct Webhooks {
    client: reqwest::Client,
    /// Tests only, never read from the environment: allows plain http and private address
    /// literals, for a receiver on 127.0.0.1. Names still resolve to public addresses only.
    pub allow_private: bool,
}

impl Default for Webhooks {
    fn default() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::new(PublicOnly))
            .user_agent("SocialReins-Webhooks")
            .build()
            .expect("the webhook http client builds");
        Self {
            client,
            allow_private: false,
        }
    }
}

/// A new signing secret, `whsec_` and 48 hex characters.
pub fn new_secret() -> String {
    format!("whsec_{}", &token::new()[..SECRET_CHARS])
}

impl Webhooks {
    /// `https://`, at most 2000 characters, no credentials, and a host that is a name other
    /// than `localhost` or a public address. Checked when saved and again when sent.
    pub fn check_url(&self, raw: &str) -> Option<Url> {
        if raw.chars().count() > MAX_URL_CHARS {
            return None;
        }
        let url = Url::parse(raw).ok()?;
        let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
        if !url.username().is_empty() || url.password().is_some() || host.is_empty() {
            return None;
        }
        if self.allow_private {
            return matches!(url.scheme(), "https" | "http").then_some(url);
        }
        let ok_host = match host.trim_matches(['[', ']']).parse::<IpAddr>() {
            Ok(ip) => public(ip),
            Err(_) => host != "localhost" && !host.ends_with(".localhost"),
        };
        (url.scheme() == "https" && ok_host).then_some(url)
    }

    /// POSTs `body`, signed; the receiver's status code, or an error code: `blocked_address`,
    /// `timeout` or `unreachable`. The response body is never read.
    pub async fn send(
        &self,
        url: &str,
        secret: &str,
        event: &str,
        id: Uuid,
        body: String,
    ) -> Result<u16, &'static str> {
        let url = self.check_url(url).ok_or("blocked_address")?;
        let signature = webhook::signature(secret, Utc::now().timestamp(), &body);
        let sent = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header("SocialReins-Signature", signature)
            .header("SocialReins-Event", event)
            .header("SocialReins-Delivery", id.to_string())
            .body(body)
            .send()
            .await;
        match sent {
            Ok(res) => Ok(res.status().as_u16()),
            Err(e) => Err(error_code(&e)),
        }
    }
}

fn error_code(e: &reqwest::Error) -> &'static str {
    if e.is_timeout() {
        return "timeout";
    }
    let mut cause: Option<&dyn std::error::Error> = Some(e);
    while let Some(c) = cause {
        if c.to_string() == NOT_PUBLIC {
            return "blocked_address";
        }
        cause = c.source();
    }
    "unreachable"
}

/// Minutes to wait after `attempts` failed tries; `None`: give up.
pub fn retry_in(attempts: i32) -> Option<i32> {
    let i = usize::try_from(attempts.checked_sub(1)?).ok()?;
    BACKOFF_MINUTES.get(i).copied()
}

/// Wraps `update`, an `UPDATE post_targets ... ` without `RETURNING`, so the same statement
/// queues `post.target.<new status>` for every changed target of a team with a webhook. The
/// result has one `post_id` row per changed target.
pub fn with_event(update: &str) -> String {
    format!(
        "WITH changed AS ({update} \
           RETURNING id, post_id, social_account_id, status, remote_url, error), \
         queued AS (INSERT INTO webhook_deliveries (id, team_id, target_id, event, payload) \
           SELECT c.did, w.team_id, c.id, 'post.target.' || c.status, jsonb_build_object( \
             'id', c.did, 'type', 'post.target.' || c.status, 'created_at', now(), \
             'data', jsonb_build_object('post_id', c.post_id, 'group_id', p.group_id, \
               'target_id', c.id, 'social_account_id', c.social_account_id, \
               'platform', p.platform, 'status', c.status, 'remote_url', c.remote_url, \
               'error', c.error)) \
           FROM (SELECT gen_random_uuid() AS did, * FROM changed) c \
           JOIN posts p ON p.id = c.post_id \
           JOIN team_webhooks w ON w.team_id = p.team_id \
           ON CONFLICT (target_id, event) DO NOTHING) \
         SELECT post_id FROM changed"
    )
}

/// Delivered or given-up deliveries older than 30 days, for the hourly purge.
pub fn purge_sql() -> String {
    format!(
        "DELETE FROM webhook_deliveries WHERE created_at < now() - interval '30 days' \
         AND (delivered_at IS NOT NULL OR attempts >= {MAX_ATTEMPTS})"
    )
}

fn sql(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// One delivery round: sends up to 50 due deliveries at once and records each outcome. Skips
/// while another instance holds the lock.
pub async fn tick(app: &AppState) -> Result<(), DbErr> {
    let lock = app.db.begin().await?;
    let row = lock
        .query_one_raw(sql(
            "SELECT pg_try_advisory_xact_lock($1) AS ok",
            vec![DELIVERY_LOCK.into()],
        ))
        .await?;
    let locked: bool = match row {
        Some(r) => r.try_get("", "ok")?,
        None => false,
    };
    if !locked {
        return Ok(());
    }
    let due = app
        .db
        .query_all_raw(sql(
            "SELECT d.id, d.event, d.payload, d.attempts, w.url, w.secret \
             FROM webhook_deliveries d JOIN team_webhooks w ON w.team_id = d.team_id \
             WHERE d.delivered_at IS NULL AND d.attempts < $1 AND d.next_attempt_at <= now() \
             ORDER BY d.next_attempt_at LIMIT $2",
            vec![MAX_ATTEMPTS.into(), BATCH.into()],
        ))
        .await?;
    let mut runs = Vec::new();
    for r in due {
        let id: Uuid = r.try_get("", "id")?;
        let event: String = r.try_get("", "event")?;
        let payload: serde_json::Value = r.try_get("", "payload")?;
        let attempts: i32 = r.try_get("", "attempts")?;
        let url: String = r.try_get("", "url")?;
        let sealed: Vec<u8> = r.try_get("", "secret")?;
        let app = app.clone();
        runs.push(tokio::spawn(async move {
            let secret = app
                .cipher
                .decrypt(&sealed)
                .and_then(|b| String::from_utf8(b).ok());
            let sent = match secret {
                Some(s) => {
                    let body = payload.to_string();
                    app.webhooks.send(&url, &s, &event, id, body).await
                }
                None => Err("secret_unreadable"),
            };
            if let Err(e) = record(&app, id, attempts + 1, sent).await {
                tracing::error!(error = %e, delivery = %id, "webhook outcome not saved");
            }
        }));
    }
    for run in runs {
        let _ = run.await;
    }
    lock.commit().await
}

/// Saves one try: delivered on a 2xx, else the next try after the backoff, or none.
async fn record(
    app: &AppState,
    id: Uuid,
    attempts: i32,
    sent: Result<u16, &'static str>,
) -> Result<(), DbErr> {
    let (delivered, status, error) = match sent {
        Ok(code) if (200..300).contains(&code) => (true, Some(i32::from(code)), None),
        Ok(code) => (false, Some(i32::from(code)), Some("http_status")),
        Err(code) => (false, None, Some(code)),
    };
    let retry = if delivered { None } else { retry_in(attempts) };
    if !delivered {
        tracing::warn!(delivery = %id, attempts, status, error, "webhook delivery failed");
    }
    let stmt = sql(
        "UPDATE webhook_deliveries SET attempts = $2, last_status = $3, last_error = $4, \
         delivered_at = CASE WHEN $5 THEN now() END, \
         next_attempt_at = CASE WHEN $6::int IS NULL THEN next_attempt_at \
           ELSE now() + make_interval(mins => $6::int) END \
         WHERE id = $1",
        vec![
            id.into(),
            attempts.into(),
            status.into(),
            error.map(str::to_owned).into(),
            delivered.into(),
            retry.into(),
        ],
    );
    app.db.execute_raw(stmt).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_must_be_public_https() {
        let w = Webhooks::default();
        for ok in [
            "https://example.com/hook",
            "https://hooks.example.com:8443/a?b=c",
            "https://8.8.8.8/x",
            "https://[2606:4700::1111]/x",
        ] {
            assert!(w.check_url(ok).is_some(), "{ok}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_CHARS));
        for bad in [
            "http://example.com/hook",
            "https://user:pw@example.com/",
            "https://user@example.com/",
            "https://localhost/x",
            "https://LOCALHOST./x",
            "https://api.localhost/x",
            "https://127.0.0.1/x",
            "https://10.0.0.1/x",
            "https://192.168.0.10/x",
            "https://169.254.169.254/latest",
            "https://100.64.0.1/x",
            "https://0.0.0.0/x",
            "https://224.0.0.1/x",
            "https://[::1]/x",
            "https://[fd00::1]/x",
            "https://[fe80::1]/x",
            "https://[::ffff:10.0.0.1]/x",
            "https://2130706433/x",
            "https://0x7f.1/x",
            "ftp://example.com/",
            "not a url",
            long.as_str(),
        ] {
            assert!(w.check_url(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn private_literals_need_the_test_flag() {
        let mut w = Webhooks::default();
        assert!(w.check_url("http://127.0.0.1:9/x").is_none());
        w.allow_private = true;
        assert!(w.check_url("http://127.0.0.1:9/x").is_some());
        assert!(w.check_url("http://u:p@127.0.0.1:9/x").is_none());
    }

    #[test]
    fn retries_back_off_then_give_up() {
        let waits: Vec<_> = (1..=MAX_ATTEMPTS).map(retry_in).collect();
        assert_eq!(
            waits,
            [1, 5, 30, 120, 360, 720, 1440]
                .map(Some)
                .into_iter()
                .chain([None])
                .collect::<Vec<_>>()
        );
        assert_eq!(retry_in(0), None);
    }

    #[test]
    fn secrets_look_like_whsec() {
        let (a, b) = (new_secret(), new_secret());
        assert!(a.starts_with("whsec_") && a.len() == 6 + SECRET_CHARS);
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn names_resolving_to_private_addresses_are_blocked() {
        use reqwest::dns::Resolve;
        // `localhost` resolves without a network; `check_url` refuses it by name as well.
        let found = PublicOnly.resolve("localhost".parse().unwrap()).await;
        let err = found.err().expect("loopback is refused");
        assert_eq!(err.to_string(), NOT_PUBLIC);
    }
}
