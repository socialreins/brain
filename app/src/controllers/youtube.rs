//! Connect a YouTube channel through the shared OAuth flow (`oauth.rs`):
//! `GET /connections/youtube/redirect`, then `GET /connect/youtube/callback` (path registered
//! with Google, fixed) swaps the code for an access token (an hour) and a refresh token, reads the
//! channel the consent picked and saves it as a `youtube` account in that team's Default set. It
//! uses the Google client of sign-in but its own redirect URI, state and cookie, so it never
//! signs anyone in. The refresh token is kept sealed in `meta`; `fresh_token` renews the access
//! token whenever a call is about to be made with one near its end.
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
    connectors::{bluesky::PostError, youtube::Token},
    models::social_accounts,
    state::AppState,
};

const PROVIDER: &str = "youtube";
/// An access token with less than this left is renewed before a call.
const RENEW_WITHIN: Duration = Duration::minutes(2);

/// `GET /connections/youtube/redirect`: owners and admins of the current team only.
async fn start(State(app): State<AppState>, t: Result<InTeam, Reply>) -> Response {
    let how = (PROVIDER, app.youtube.configured());
    oauth::start(&app, t, how, |uri, state| {
        app.youtube.authorize_url(uri, state)
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

/// The access token to call YouTube with: the stored one, or, when it ends within two minutes
/// (or has no end recorded), a new one from the refresh token, saved before it is used. A
/// refresh token Google refuses, or none, is `Reconnect`. Two calls renewing at once both get a
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
    let token = app.youtube.refresh(&refresh).await?;
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "UPDATE social_accounts SET access_token = $2, \
         token_expires_at = now() + make_interval(secs => $3), updated_at = now() \
         WHERE id = $1 AND access_token = $4",
        [
            account.id.into(),
            app.cipher.encrypt(token.access_token.as_bytes()).into(),
            (token.expires_in as f64).into(),
            account.access_token.clone().into(),
        ],
    );
    // The token is good even when it could not be saved: the next call renews again.
    if let Err(e) = app.db.execute_raw(stmt).await {
        tracing::warn!(error = %e, account = %account.id, "youtube token save failed");
    }
    Ok(token.access_token)
}

/// What the callback needs of Google: the tokens and the channel, or the error code to land with.
async fn fetch(
    app: &AppState,
    code: &str,
) -> Result<(Token, String, crate::connectors::youtube::Channel), &'static str> {
    let exchanged = app
        .youtube
        .exchange(code, &oauth::redirect_uri(app, PROVIDER))
        .await;
    let token = match exchanged {
        Ok(t) => t,
        Err(e) => {
            log(&e, "token swap");
            return Err("youtube_failed");
        }
    };
    // `prompt=consent` always brings one; without it the account could not be renewed.
    let Some(refresh) = token.refresh_token.clone() else {
        tracing::warn!("youtube sent no refresh token");
        return Err("youtube_failed");
    };
    match app.youtube.channel(&token.access_token).await {
        Ok(Some(channel)) => Ok((token, refresh, channel)),
        Ok(None) => Err("no_channel"),
        Err(e) => {
            log(&e, "channel lookup");
            Err("youtube_failed")
        }
    }
}

fn log(e: &PostError, step: &str) {
    match e {
        PostError::Failed(why) => tracing::warn!(why, step, "youtube connect failed"),
        PostError::Reconnect => tracing::warn!(step, "youtube refused the connect"),
    }
}

/// `GET /connect/youtube/callback`: Google sends the browser here with `code` and `state`, or
/// with `error` when the user said no.
async fn callback(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> Response {
    let how = (PROVIDER, app.youtube.configured());
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
    let (token, refresh, channel) = match fetch(&app, &code).await {
        Ok(found) => found,
        Err(code) => return fail(code),
    };
    let now = Utc::now().fixed_offset();
    let new_id = Uuid::now_v7();
    let row = social_accounts::ActiveModel {
        id: Set(new_id),
        team_id: Set(team_id),
        provider: Set(PROVIDER.into()),
        external_id: Set(channel.id),
        handle: Set(channel.handle),
        display_name: Set(channel.title),
        avatar_url: Set(channel.thumbnail),
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
        tracing::error!(error = %e, "youtube account save failed");
        return fail("internal");
    }
    oauth::landed(&app, PROVIDER, &[("connected", PROVIDER)])
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/connections/youtube/redirect", get(start))
        .route("/connect/youtube/callback", get(callback))
}
