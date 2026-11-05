//! Connect a TikTok account through the shared OAuth flow (`oauth.rs`):
//! `GET /connections/tiktok/redirect`, then `GET /connect/tiktok/callback` (path registered
//! with TikTok, fixed) swaps the code for an access token (a day) and a refresh token (a year),
//! reads the account the consent picked and saves it as a `tiktok` account in that team's
//! Default set. The refresh token is kept sealed in `meta`; `fresh_token` renews the access
//! token whenever a call is about to be made with one near its end, and saves the refresh token
//! TikTok sends back with it, if it is a new one.
use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::Response,
    routing::get,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{Duration, Utc};
use sea_orm::{ActiveValue::Set, ConnectionTrait, DbBackend, Statement};
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

/// The refresh token sealed in an account's `meta`.
fn unseal(app: &AppState, account: &social_accounts::Model) -> Option<String> {
    let sealed = account.meta["refresh_token"].as_str()?;
    let bytes = URL_SAFE_NO_PAD.decode(sealed).ok()?;
    String::from_utf8(app.cipher.decrypt(&bytes)?).ok()
}

/// The access token to call TikTok with: the stored one, or, when it ends within ten minutes
/// (or has no end recorded), a new one from the refresh token, saved before it is used. A
/// refresh token TikTok refuses, or none, is `Reconnect`. Two calls renewing at once both get a
/// working token; the later save finds the row changed and leaves it.
pub async fn fresh_token(
    app: &AppState,
    account: &social_accounts::Model,
) -> Result<String, PostError> {
    let stored = app
        .cipher
        .decrypt(&account.access_token)
        .and_then(|b| String::from_utf8(b).ok());
    let live = account
        .token_expires_at
        .is_some_and(|t| t > Utc::now() + RENEW_WITHIN);
    if let (true, Some(token)) = (live, &stored) {
        return Ok(token.clone());
    }
    let Some(refresh) = unseal(app, account) else {
        return Err(PostError::Reconnect);
    };
    let token = app.tiktok.refresh(&refresh).await?;
    let next = token.refresh_token.clone().unwrap_or(refresh);
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE social_accounts SET access_token = $2, \
         meta = meta || jsonb_build_object('refresh_token', $5::text), \
         token_expires_at = now() + make_interval(secs => $3), updated_at = now() \
         WHERE id = $1 AND access_token = $4",
        [
            account.id.into(),
            app.cipher.encrypt(token.access_token.as_bytes()).into(),
            (token.expires_in as f64).into(),
            account.access_token.clone().into(),
            seal(app, &next).into(),
        ],
    );
    // The token is good even when it could not be saved: the next call renews again.
    if let Err(e) = app.db.execute_raw(stmt).await {
        tracing::warn!(error = %e, account = %account.id, "tiktok token save failed");
    }
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
