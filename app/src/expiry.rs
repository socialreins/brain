//! When an account's token ends or the platform refused it, and who hears about it.
//!
//! Nothing new is stored for the status: it is computed from `token_expires_at` and
//! `meta.reconnect` (set only when a token is found dead: a refused renewal, or a main publish
//! call the platform answers with a dead-token signal; a refused permission or a failing side
//! call such as a cover, a first comment or a take down never sets it; every connect flow
//! replaces `meta`, so reconnecting clears it). The daily mail keeps its own once-per-cycle mark
//! in `meta.expiry_notice` for the same reason: a reconnect or a renewal starts a new cycle.
//!
//! The first run mails for every account already flagged or past due. A data import that brings
//! such accounts in must set `meta.expiry_notice` (`expired` or `expiring`) on them first, or
//! their teams get a burst of mail.
use chrono::{DateTime, Duration, Utc};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, EntityTrait, Statement, TransactionTrait, Value};
use serde::Serialize;
use socialreins_core::team::inert;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{models::social_accounts, state::AppState};

/// How far ahead of the end of a token that cannot renew itself the user is warned.
pub const WARN_DAYS: i64 = 7;
const LOCK: i64 = 0x0045_5850_4952_5944;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    /// The token ends within 7 days and nothing renews it.
    Expiring,
    /// The token expired or the platform refused it: connect the account again.
    Reconnect,
}

/// X, TikTok and YouTube renew an access token from the refresh token whenever it is used, so
/// its short end date is normal. Every other token ends for good (Threads and LinkedIn are
/// extended by a daily job well before the last week, so a token that near its end means that
/// job failed).
fn renews_on_use(provider: &str) -> bool {
    matches!(provider, "x" | "tiktok" | "youtube")
}

pub fn status(a: &social_accounts::Model, now: DateTime<Utc>) -> Status {
    if a.meta["reconnect"].as_bool() == Some(true) {
        return Status::Reconnect;
    }
    let Some(end) = a.token_expires_at.filter(|_| !renews_on_use(&a.provider)) else {
        return Status::Ok;
    };
    if end <= now {
        Status::Reconnect
    } else if end <= now + Duration::days(WARN_DAYS) {
        Status::Expiring
    } else {
        Status::Ok
    }
}

/// Remembers that the platform refused this account's credentials. Idempotent, and it leaves
/// `updated_at` alone. A failure is logged: the post that hit it already carries the message.
pub async fn flag_reconnect(db: &impl ConnectionTrait, id: Uuid) {
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE social_accounts SET meta = meta || '{\"reconnect\": true}'::jsonb \
         WHERE id = $1 AND COALESCE(meta->>'reconnect', 'false') <> 'true'",
        [id.into()],
    );
    if let Err(e) = db.execute_raw(stmt).await {
        tracing::warn!(error = %e, account = %id, "reconnect flag not saved");
    }
}

fn label(provider: &str) -> &str {
    match provider {
        "bluesky" => "Bluesky",
        "facebook" => "Facebook",
        "instagram" => "Instagram",
        "threads" => "Threads",
        "linkedin" => "LinkedIn",
        "x" => "X",
        "tiktok" => "TikTok",
        "youtube" => "YouTube",
        other => other,
    }
}

fn stmt(sql: &str, values: impl IntoIterator<Item = Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// Emails the owners and admins of each team about an account whose token ends within 7 days
/// ("expiring") and again once it ended or was refused ("expired"): once per level per account
/// until it is reconnected or renewed. One instance at a time (0 while another holds the lock).
/// Returns how many accounts were announced.
pub async fn notify(app: &AppState) -> Result<u64, DbErr> {
    let lock = app.db.begin().await?;
    let row = lock
        .query_one_raw(stmt(
            "SELECT pg_try_advisory_xact_lock($1) AS ok",
            [LOCK.into()],
        ))
        .await?;
    let locked: bool = match row {
        Some(r) => r.try_get("", "ok")?,
        None => false,
    };
    if !locked {
        return Ok(0);
    }
    let rows = social_accounts::Entity::find()
        .from_raw_sql(stmt(
            "SELECT * FROM social_accounts WHERE meta->>'reconnect' = 'true' \
             OR meta->>'expiry_notice' IS NOT NULL \
             OR (token_expires_at <= now() + make_interval(days => $1) \
                 AND provider NOT IN ('x', 'tiktok', 'youtube'))",
            [(WARN_DAYS as i32).into()],
        ))
        .all(&app.db)
        .await?;
    let now = Utc::now();
    let mut done = 0;
    for a in rows {
        let level = match status(&a, now) {
            Status::Ok => {
                // Renewed since the last notice: the next end is a new cycle.
                if a.meta.get("expiry_notice").is_some() {
                    release(app, &a).await?;
                }
                continue;
            }
            Status::Expiring => "expiring",
            Status::Reconnect => "expired",
        };
        let sent = a.meta["expiry_notice"].as_str();
        if sent == Some(level) || (sent == Some("expired") && level == "expiring") {
            continue;
        }
        // The claim: only one instance wins it, and a reconnect meanwhile (new token) voids it.
        let claim = stmt(
            "UPDATE social_accounts SET meta = meta || jsonb_build_object('expiry_notice', $2::text) \
             WHERE id = $1 AND access_token = $3 AND meta->>'expiry_notice' IS DISTINCT FROM $2",
            [
                a.id.into(),
                level.into(),
                a.access_token.clone().into(),
            ],
        );
        if app.db.execute_raw(claim).await?.rows_affected() != 1 {
            continue;
        }
        // Nobody mailed, or the recipients could not be read: release the claim so the next
        // run tries again.
        match tell_admins(app, &a, level).await {
            Ok(true) => done += 1,
            Ok(false) => release(app, &a).await?,
            Err(e) => {
                tracing::warn!(error = %e, account = %a.id, "expiry mail recipients not read");
                release(app, &a).await?;
            }
        }
    }
    lock.commit().await?;
    Ok(done)
}

/// Forgets the notice so the next run tries again (nobody could be mailed) or so a renewed
/// token starts a new cycle.
async fn release(app: &AppState, a: &social_accounts::Model) -> Result<(), DbErr> {
    let undo = stmt(
        "UPDATE social_accounts SET meta = meta - 'expiry_notice' \
         WHERE id = $1 AND access_token = $2",
        [a.id.into(), a.access_token.clone().into()],
    );
    app.db.execute_raw(undo).await.map(|_| ())
}

/// Mails the team's owners and admins. False when no mail went out, so the claim is released.
async fn tell_admins(
    app: &AppState,
    a: &social_accounts::Model,
    level: &str,
) -> Result<bool, DbErr> {
    let to = app
        .db
        .query_all_raw(stmt(
            "SELECT u.email FROM team_user t JOIN users u ON u.id = t.user_id \
             WHERE t.team_id = $1 AND t.role IN ('owner', 'admin') ORDER BY u.email",
            [a.team_id.into()],
        ))
        .await?;
    let platform = label(&a.provider);
    let who = a
        .display_name
        .as_deref()
        .or(a.handle.as_deref())
        .map(inert)
        .map(|n| format!(" ({})", n.trim()))
        .unwrap_or_default();
    let (subject, lead) = match (level, a.token_expires_at) {
        ("expiring", Some(end)) => (
            format!(
                "Your {platform} connection ends on {}",
                end.format("%Y-%m-%d")
            ),
            format!(
                "The {platform} account{who} on SocialReins stops working on {} and {platform} \
                 gives no way to extend it.",
                end.format("%Y-%m-%d")
            ),
        ),
        _ => (
            format!("Reconnect your {platform} account on SocialReins"),
            format!(
                "The {platform} account{who} on SocialReins no longer works: its token expired \
                 or {platform} refused it. Posts to it fail until you reconnect."
            ),
        ),
    };
    let text = format!(
        "{lead}\n\nOn the Connections page, choose Reconnect.\n\nOpen Connections:\n\n{}/app/connections\n",
        app.web_url
    );
    let mut sent = false;
    for r in to {
        let email: String = r.try_get("", "email")?;
        match app.mailer.send(&email, &subject, text.clone()).await {
            Ok(()) => sent = true,
            Err(e) => tracing::warn!(error = %e, account = %a.id, "expiry mail not sent"),
        }
    }
    Ok(sent)
}
