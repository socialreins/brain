//! Connect an X account through the shared OAuth flow (`oauth.rs`), with PKCE:
//! `GET /connections/x/redirect` keeps a fresh verifier sealed with the state (never in the
//! URL), then `GET /connect/x/callback` (path registered with X, fixed) swaps the code and
//! verifier for an access token (two hours) and a refresh token, reads the account and saves it
//! as an `x` account in that team's Default set. The refresh token is kept sealed in `meta`.
//!
//! X refresh tokens are single use, so `fresh_token` renews under a lock on the account row and
//! saves the new refresh token in the same transaction as the new access token; two calls
//! renewing at once spend it once. A refresh X refuses sets `meta.reconnect`, which stays until
//! the user connects the account again. A process that dies between X's answer and the commit
//! loses the new refresh token, and the account then asks for a reconnect.
use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::Response,
    routing::get,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Duration, Utc};
use sea_orm::{ActiveValue::Set, ConnectionTrait, DbBackend, DbErr, Statement, TransactionTrait};
use socialreins_core::{oauth::pkce_challenge, token};
use uuid::Uuid;

use super::{
    connections::save_account,
    oauth::{self, Callback},
    teams::InTeam,
    waitlist::Reply,
};
use crate::{
    connectors::{
        bluesky::PostError,
        x::{Profile, Token},
    },
    models::social_accounts,
    state::AppState,
};

const PROVIDER: &str = "x";
/// An access token with less than this left is renewed before a call.
const RENEW_WITHIN: Duration = Duration::minutes(2);
/// The job renews tokens with less than this left, so a publish finds a fresh one.
const JOB_RENEW_WITHIN: Duration = Duration::minutes(30);
/// Advisory lock key held by the instance running the renewal job.
const REFRESH_LOCK: i64 = 0x0058_5245_4652_4553;

fn seal(app: &AppState, secret: &str) -> String {
    URL_SAFE_NO_PAD.encode(app.cipher.encrypt(secret.as_bytes()))
}

fn unseal_str(app: &AppState, sealed: &str) -> Option<String> {
    let bytes = URL_SAFE_NO_PAD.decode(sealed).ok()?;
    String::from_utf8(app.cipher.decrypt(&bytes)?).ok()
}

/// `GET /connections/x/redirect`: owners and admins of the current team only.
async fn start(State(app): State<AppState>, t: Result<InTeam, Reply>) -> Response {
    let how = (PROVIDER, app.x.configured());
    let verifier = token::new();
    let challenge = pkce_challenge(&verifier);
    let sealed = seal(&app, &verifier);
    oauth::start_with(&app, t, how, &sealed, |uri, state| {
        app.x.authorize_url(uri, state, &challenge)
    })
    .await
}

/// What the callback needs of X: the tokens and the account, or the error code to land with.
async fn fetch(
    app: &AppState,
    code: &str,
    verifier: &str,
) -> Result<(Token, String, Profile), &'static str> {
    let exchanged = app
        .x
        .exchange(code, &oauth::redirect_uri(app, PROVIDER), verifier)
        .await;
    let token = match exchanged {
        Ok(t) => t,
        Err(e) => {
            log(&e, "token swap");
            return Err("x_failed");
        }
    };
    // The consent screen lets the user untick scopes; without all of them, publishing, media or
    // taking down would fail later, so nothing is saved.
    if !token.has_all_scopes() {
        tracing::warn!("x consent lacked a needed scope");
        return Err("missing_scopes");
    }
    // `offline.access` always brings one; without it the account could not be renewed.
    let Some(refresh) = token.refresh_token.clone() else {
        tracing::warn!("x sent no refresh token");
        return Err("x_failed");
    };
    match app.x.me(&token.access_token).await {
        Ok(me) => Ok((token, refresh, me)),
        Err(e) => {
            log(&e, "profile");
            Err("x_failed")
        }
    }
}

fn log(e: &PostError, step: &str) {
    match e {
        PostError::Failed(why) => tracing::warn!(why, step, "x connect failed"),
        PostError::Reconnect => tracing::warn!(step, "x refused the connect"),
    }
}

/// `GET /connect/x/callback`: X sends the browser here with `code` and `state`, or with `error`
/// when the user said no.
async fn callback(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> Response {
    let how = (PROVIDER, app.x.configured());
    let (team_id, user_id, code, sealed) = match oauth::finish_with(&app, &headers, q, how).await {
        Ok(found) => found,
        Err(landing) => return *landing,
    };
    let fail = |code| {
        oauth::landed(
            &app,
            PROVIDER,
            &[("connect_error", code), ("provider", PROVIDER)],
        )
    };
    // A verifier that cannot be read is an attempt that cannot finish.
    let Some(verifier) = unseal_str(&app, &sealed).filter(|v| !v.is_empty()) else {
        return fail("expired");
    };
    let (token, refresh, me) = match fetch(&app, &code, &verifier).await {
        Ok(found) => found,
        Err(code) => return fail(code),
    };
    let now = Utc::now().fixed_offset();
    let new_id = Uuid::now_v7();
    let name = me.name.filter(|n| !n.trim().is_empty());
    let row = social_accounts::ActiveModel {
        id: Set(new_id),
        team_id: Set(team_id),
        provider: Set(PROVIDER.into()),
        external_id: Set(me.id),
        handle: Set(Some(me.username)),
        display_name: Set(name),
        avatar_url: Set(me.profile_image_url),
        access_token: Set(app.cipher.encrypt(token.access_token.as_bytes())),
        token_expires_at: Set(Some(now + Duration::seconds(token.expires_in))),
        meta: Set(serde_json::json!({ "refresh_token": seal(&app, &refresh) })),
        dm_access: Set(false),
        inbox_error_at: Set(None),
        created_by: Set(Some(user_id)),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    };
    // A reconnect keeps the row and takes the new tokens and profile, and clears the flag.
    let refresh = [
        social_accounts::Column::Handle,
        social_accounts::Column::DisplayName,
        social_accounts::Column::AvatarUrl,
        social_accounts::Column::AccessToken,
        social_accounts::Column::TokenExpiresAt,
        social_accounts::Column::Meta,
        social_accounts::Column::UpdatedAt,
    ];
    if let Err(e) = save_account(&app.db, (team_id, new_id), row, refresh).await {
        tracing::error!(error = %e, "x account save failed");
        return fail("internal");
    }
    oauth::landed(&app, PROVIDER, &[("connected", PROVIDER)])
}

fn stmt(sql: &str, values: Vec<sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

fn db_failed(e: DbErr) -> PostError {
    tracing::error!(error = %e, "x token save failed");
    PostError::Failed("The X token could not be renewed. Try again.".into())
}

/// The access token to call X with: the stored one, or, when it ends within two minutes (or has
/// no end recorded), a new one (see `renew`). An account flagged `reconnect`, or one X refuses
/// to renew, is `Reconnect`.
pub async fn fresh_token(
    app: &AppState,
    account: &social_accounts::Model,
) -> Result<String, PostError> {
    if account.meta["reconnect"].as_bool() == Some(true) {
        return Err(PostError::Reconnect);
    }
    let live = account
        .token_expires_at
        .is_some_and(|t| t > Utc::now() + RENEW_WITHIN);
    let stored = app
        .cipher
        .decrypt(&account.access_token)
        .and_then(|b| String::from_utf8(b).ok());
    if let (true, Some(token)) = (live, stored) {
        return Ok(token);
    }
    renew(app, account.id, RENEW_WITHIN).await.map(|r| r.0)
}

/// Locks the account row, then renews its token unless it already ends later than `within` from
/// now (someone else renewed while this waited for the lock). Returns the token and whether X
/// was called. The new access token and refresh token are saved in the transaction that holds
/// the lock; a response without a refresh token keeps the current one. A refused refresh sets
/// `meta.reconnect` and is `Reconnect`; any other failure leaves the row as it was.
async fn renew(app: &AppState, id: Uuid, within: Duration) -> Result<(String, bool), PostError> {
    let txn = app.db.begin().await.map_err(db_failed)?;
    let row = txn
        .query_one_raw(stmt(
            "SELECT access_token, token_expires_at, meta FROM social_accounts \
             WHERE id = $1 FOR UPDATE",
            vec![id.into()],
        ))
        .await
        .map_err(db_failed)?;
    // The account was disconnected meanwhile.
    let Some(row) = row else {
        return Err(PostError::Reconnect);
    };
    let sealed: Vec<u8> = row.try_get("", "access_token").map_err(db_failed)?;
    let expires: Option<DateTime<Utc>> = row.try_get("", "token_expires_at").map_err(db_failed)?;
    let meta: serde_json::Value = row.try_get("", "meta").map_err(db_failed)?;
    if meta["reconnect"].as_bool() == Some(true) {
        return Err(PostError::Reconnect);
    }
    let live = expires.is_some_and(|t| t > Utc::now() + within);
    let stored = app
        .cipher
        .decrypt(&sealed)
        .and_then(|b| String::from_utf8(b).ok());
    if let (true, Some(token)) = (live, stored) {
        return Ok((token, false));
    }
    let current = meta["refresh_token"]
        .as_str()
        .and_then(|s| unseal_str(app, s));
    let refreshed = match &current {
        Some(old) => app.x.refresh(old).await,
        None => Err(PostError::Reconnect),
    };
    match refreshed {
        Ok(token) => {
            let next = token.refresh_token.clone().or(current).unwrap_or_default();
            let meta = serde_json::json!({ "refresh_token": seal(app, &next) });
            txn.execute_raw(stmt(
                "UPDATE social_accounts SET access_token = $2, meta = $4, \
                 token_expires_at = now() + make_interval(secs => $3), updated_at = now() \
                 WHERE id = $1",
                vec![
                    id.into(),
                    app.cipher.encrypt(token.access_token.as_bytes()).into(),
                    (token.expires_in as f64).into(),
                    meta.into(),
                ],
            ))
            .await
            .map_err(db_failed)?;
            txn.commit().await.map_err(db_failed)?;
            Ok((token.access_token, true))
        }
        Err(PostError::Reconnect) => {
            tracing::warn!(account = %id, "x refused the token refresh");
            txn.execute_raw(stmt(
                "UPDATE social_accounts SET meta = meta || '{\"reconnect\": true}'::jsonb, \
                 updated_at = now() WHERE id = $1",
                vec![id.into()],
            ))
            .await
            .map_err(db_failed)?;
            txn.commit().await.map_err(db_failed)?;
            Err(PostError::Reconnect)
        }
        // Dropping the transaction leaves the row as it was.
        Err(e) => Err(e),
    }
}

/// Renews every X token with 30 minutes or less left (or already expired) for accounts not
/// flagged for a reconnect, so a publish finds a fresh one. Returns how many were renewed; a
/// refused refresh flags the account (see `renew`), another failure is logged and retried at
/// the next run. One instance at a time (0 while another holds the lock).
pub async fn refresh_tokens(app: &AppState) -> Result<u64, DbErr> {
    let lock = app.db.begin().await?;
    let row = lock
        .query_one_raw(stmt(
            "SELECT pg_try_advisory_xact_lock($1) AS ok",
            vec![REFRESH_LOCK.into()],
        ))
        .await?;
    let locked: bool = match row {
        Some(r) => r.try_get("", "ok")?,
        None => false,
    };
    if !locked {
        return Ok(0);
    }
    let rows = app
        .db
        .query_all_raw(stmt(
            "SELECT id FROM social_accounts \
             WHERE provider = $1 AND meta->>'refresh_token' IS NOT NULL \
             AND COALESCE(meta->>'reconnect', 'false') <> 'true' \
             AND (token_expires_at IS NULL \
                  OR token_expires_at <= now() + make_interval(secs => $2))",
            vec![
                PROVIDER.into(),
                (JOB_RENEW_WITHIN.num_seconds() as f64).into(),
            ],
        ))
        .await?;
    let mut done = 0;
    for row in rows {
        let id: Uuid = row.try_get("", "id")?;
        match renew(app, id, JOB_RENEW_WITHIN).await {
            Ok((_, true)) => done += 1,
            Ok((_, false)) | Err(PostError::Reconnect) => {}
            Err(PostError::Failed(why)) => {
                tracing::warn!(account = %id, why, "x token refresh failed");
            }
        }
    }
    lock.commit().await?;
    Ok(done)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/connections/x/redirect", get(start))
        .route("/connect/x/callback", get(callback))
}
