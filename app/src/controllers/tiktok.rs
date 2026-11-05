//! Connect a TikTok account through the shared OAuth flow (`oauth.rs`):
//! `GET /connections/tiktok/redirect`, then `GET /connect/tiktok/callback` (path registered
//! with TikTok, fixed) swaps the code for an access token (a day) and a refresh token (a year),
//! reads the account the consent picked and saves it as a `tiktok` account in that team's
//! Default set. The refresh token is kept sealed in `meta`; `fresh_token` renews the access
//! token under a lock on the account row whenever a call is about to be made with one near its
//! end, and saves the refresh token TikTok sends back with it in the same transaction.
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
        tiktok::{Profile, Token},
    },
    models::social_accounts,
    state::AppState,
};

const PROVIDER: &str = "tiktok";
/// An access token with less than this left is renewed before a call. One post can take up to
/// 5 minutes (`PUBLISH_TIMEOUT`), so this leaves room for it.
const RENEW_WITHIN: Duration = Duration::minutes(10);

/// `GET /connections/tiktok/redirect`: owners and admins of the current team only.
async fn start(State(app): State<AppState>, t: Result<InTeam, Reply>) -> Response {
    let how = (PROVIDER, app.tiktok.configured());
    oauth::start(&app, t, how, |uri, state| {
        app.tiktok.authorize_url(uri, state)
    })
    .await
}

fn seal(app: &AppState, refresh_token: &str) -> String {
    URL_SAFE_NO_PAD.encode(app.cipher.encrypt(refresh_token.as_bytes()))
}

fn stmt(sql: &str, values: Vec<sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

fn db_failed(e: DbErr) -> PostError {
    tracing::error!(error = %e, "tiktok token save failed");
    PostError::Failed("The TikTok token could not be renewed. Try again.".into())
}

/// The access token to call TikTok with: the stored one, or, when it ends within ten minutes
/// (or has no end recorded), a new one from the refresh token. It locks the account row and
/// reads it again rather than trusting `account`, so two calls renewing at once spend the refresh
/// token once: TikTok may answer a refresh with a new refresh token and drop the old one, so the
/// new one is saved in the same transaction as the new access token, and a save that fails fails
/// the call. A refresh token TikTok refuses, or none, is `Reconnect`.
pub async fn fresh_token(
    app: &AppState,
    account: &social_accounts::Model,
) -> Result<String, PostError> {
    let txn = app.db.begin().await.map_err(db_failed)?;
    let row = txn
        .query_one_raw(stmt(
            "SELECT access_token, token_expires_at, meta FROM social_accounts \
             WHERE id = $1 FOR UPDATE",
            vec![account.id.into()],
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
    let live = expires.is_some_and(|t| t > Utc::now() + RENEW_WITHIN);
    let stored = app
        .cipher
        .decrypt(&sealed)
        .and_then(|b| String::from_utf8(b).ok());
    if let (true, Some(token)) = (live, stored) {
        return Ok(token);
    }
    let Some(refresh) = meta["refresh_token"]
        .as_str()
        .and_then(|s| URL_SAFE_NO_PAD.decode(s).ok())
        .and_then(|b| app.cipher.decrypt(&b))
        .and_then(|b| String::from_utf8(b).ok())
    else {
        return Err(PostError::Reconnect);
    };
    let token = app.tiktok.refresh(&refresh).await?;
    let next = token.refresh_token.clone().unwrap_or(refresh);
    txn.execute_raw(stmt(
        "UPDATE social_accounts SET access_token = $2, \
         meta = meta || jsonb_build_object('refresh_token', $4::text), \
         token_expires_at = now() + make_interval(secs => $3), updated_at = now() \
         WHERE id = $1",
        vec![
            account.id.into(),
            app.cipher.encrypt(token.access_token.as_bytes()).into(),
            (token.expires_in as f64).into(),
            seal(app, &next).into(),
        ],
    ))
    .await
    .map_err(db_failed)?;
    txn.commit().await.map_err(db_failed)?;
    Ok(token.access_token)
}

/// What the callback needs of TikTok: the tokens and the account, or the error code to land with.
async fn fetch(app: &AppState, code: &str) -> Result<(Token, String, Profile), &'static str> {
    let exchanged = app
        .tiktok
        .exchange(code, &oauth::redirect_uri(app, PROVIDER))
        .await;
    let token = match exchanged {
        Ok(t) => t,
        Err(e) => {
            log(&e, "token swap");
            return Err("tiktok_failed");
        }
    };
    // The consent screen lets the user untick scopes; without all of them, posting would fail
    // later, so nothing is saved.
    if !token.has_all_scopes() {
        tracing::warn!("tiktok consent lacked a needed scope");
        return Err("missing_scopes");
    }
    // The code swap always brings one; without it the account could not be renewed.
    let Some(refresh) = token.refresh_token.clone() else {
        tracing::warn!("tiktok sent no refresh token");
        return Err("tiktok_failed");
    };
    match app.tiktok.profile(&token.access_token).await {
        Ok(me) => Ok((token, refresh, me)),
        Err(e) => {
            log(&e, "profile");
            Err("tiktok_failed")
        }
    }
}

fn log(e: &PostError, step: &str) {
    match e {
        PostError::Failed(why) => tracing::warn!(why, step, "tiktok connect failed"),
        PostError::Reconnect => tracing::warn!(step, "tiktok refused the connect"),
    }
}

/// `GET /connect/tiktok/callback`: TikTok sends the browser here with `code` and `state`, or
/// with `error` when the user said no.
async fn callback(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> Response {
    let how = (PROVIDER, app.tiktok.configured());
    let (team_id, user_id, code) = match oauth::finish(&app, &headers, q, how).await {
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
    let (token, refresh, me) = match fetch(&app, &code).await {
        Ok(found) => found,
        Err(code) => return fail(code),
    };
    let now = Utc::now().fixed_offset();
    let new_id = Uuid::now_v7();
    let row = social_accounts::ActiveModel {
        id: Set(new_id),
        team_id: Set(team_id),
        provider: Set(PROVIDER.into()),
        external_id: Set(me.open_id),
        handle: Set(me.username),
        display_name: Set(me.display_name),
        avatar_url: Set(me.avatar_url),
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
    // A reconnect keeps the row and takes the new tokens and profile.
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
        tracing::error!(error = %e, "tiktok account save failed");
        return fail("internal");
    }
    oauth::landed(&app, PROVIDER, &[("connected", PROVIDER)])
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/connections/tiktok/redirect", get(start))
        .route("/connect/tiktok/callback", get(callback))
}
