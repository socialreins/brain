//! Connect a Threads profile. `GET /connections/threads/redirect` (a page navigation from web,
//! with the session cookie) sends an owner or admin of the current team to Threads with a fresh
//! state, stored server-side with that team and user for 10 minutes, its hash only, while the
//! browser holds the state in a cookie. `GET /connect/threads/callback` (path registered with
//! Threads, fixed) checks the cookie against the state in constant time, uses the state once,
//! checks the same user is signed in and still manages that team, swaps the code for a 60-day
//! token, reads the profile and saves it as a `threads` account in that team's Default set.
//! Both land on `{WEB_URL}/app/connections`: `?connected=threads`, or
//! `?connect_error=<code>&provider=threads` with no details.
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap},
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use chrono::{Duration, Utc};
use sea_orm::{ActiveValue::Set, ConnectionTrait, DbBackend, DbErr, Statement, TransactionTrait};
use serde::Deserialize;
use socialreins_core::token;
use uuid::Uuid;

use super::{
    auth::{cookie_value, named_cookie, signed_in},
    connections::save_account,
    teams::InTeam,
    waitlist::Reply,
};
use crate::{connectors::bluesky::PostError, models::social_accounts, state::AppState};

const PROVIDER: &str = "threads";
const STATE_COOKIE: &str = "sr_threads";
const COOKIE_PATH: &str = "/connect/threads";
const STATE_MINUTES: i64 = 10;
/// Advisory lock key held by the instance refreshing tokens.
const REFRESH_LOCK: i64 = 0x0054_4852_4541_4453;

fn redirect_uri(app: &AppState) -> String {
    format!("{}/connect/threads/callback", app.brain_url)
}

/// Back to the web app's connections page, with how it went.
fn back(app: &AppState, error: Option<&str>) -> Response {
    let page = format!("{}/app/connections", app.web_url);
    let params = match error {
        None => vec![("connected", PROVIDER)],
        Some(code) => vec![("connect_error", code), ("provider", PROVIDER)],
    };
    let to = reqwest::Url::parse_with_params(&page, params).map_or(page, String::from);
    Redirect::to(&to).into_response()
}

/// `GET /connections/threads/redirect`: owners and admins of the current team only.
async fn start(State(app): State<AppState>, t: Result<InTeam, Reply>) -> Response {
    if !app.threads.configured() {
        return back(&app, Some("not_configured"));
    }
    let Ok(t) = t else {
        return back(&app, Some("unauthorized"));
    };
    if !t.role.manages() {
        return back(&app, Some("forbidden"));
    }
    if !app.connect_limiter.hit(t.current.0.id) {
        return back(&app, Some("rate_limited"));
    }
    let state = token::new();
    let saved = async {
        app.db
            .execute_unprepared("DELETE FROM oauth_states WHERE expires_at < now()")
            .await?;
        let stmt = Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO oauth_states \
             (state_hash, nonce, verifier, next, expires_at, provider, team_id, user_id) \
             VALUES ($1, '', '', '', $2, $3, $4, $5)",
            [
                token::hash(&state).into(),
                (Utc::now() + Duration::minutes(STATE_MINUTES))
                    .fixed_offset()
                    .into(),
                PROVIDER.into(),
                t.team.id.into(),
                t.current.0.id.into(),
            ],
        );
        app.db.execute_raw(stmt).await.map(|_| ())
    };
    if let Err(e) = saved.await {
        tracing::error!(error = %e, "threads state insert failed");
        return back(&app, Some("internal"));
    }
    let Some(url) = app.threads.authorize_url(&redirect_uri(&app), &state) else {
        return back(&app, Some("not_configured"));
    };
    let max_age = Duration::minutes(STATE_MINUTES).num_seconds();
    let cookie = named_cookie(&app, STATE_COOKIE, COOKIE_PATH, &state, max_age);
    ([(header::SET_COOKIE, cookie)], Redirect::to(&url)).into_response()
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
}

/// The attempt stored for `state`, deleted as it is read so it works once: `(team, user)` when
/// it is live and the user still manages the team.
async fn take_state(app: &AppState, state: &str) -> Result<Option<(Uuid, Uuid)>, DbErr> {
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "DELETE FROM oauth_states s WHERE state_hash = $1 AND provider = $2 \
         RETURNING team_id, user_id, expires_at > now() AND EXISTS ( \
           SELECT 1 FROM team_user m WHERE m.team_id = s.team_id AND m.user_id = s.user_id \
           AND m.role IN ('owner', 'admin')) AS ok",
        [token::hash(state).into(), PROVIDER.into()],
    );
    let Some(row) = app.db.query_one_raw(stmt).await? else {
        return Ok(None);
    };
    let ok: bool = row.try_get("", "ok")?;
    if !ok {
        return Ok(None);
    }
    Ok(Some((
        row.try_get("", "team_id")?,
        row.try_get("", "user_id")?,
    )))
}

/// `GET /connect/threads/callback`: Threads sends the browser here with `code` and `state`, or
/// with `error` when the user said no.
async fn callback(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> Response {
    let clear = named_cookie(&app, STATE_COOKIE, COOKIE_PATH, "", 0);
    let fail = |code: &str| {
        (
            [(header::SET_COOKIE, clear.clone())],
            back(&app, Some(code)),
        )
            .into_response()
    };
    if !app.threads.configured() {
        return fail("not_configured");
    }
    let Some(state) = q.state else {
        return fail("expired");
    };
    // The browser that started the connect must be the one finishing it.
    let from_this_browser =
        cookie_value(&headers, STATE_COOKIE).is_some_and(|c| token::same(c, &state));
    if !from_this_browser {
        return fail("expired");
    }
    let (team_id, user_id) = match take_state(&app, &state).await {
        Ok(Some(found)) => found,
        Ok(None) => return fail("expired"),
        Err(e) => {
            tracing::error!(error = %e, "threads state lookup failed");
            return fail("internal");
        }
    };
    let Some(code) = q.code.filter(|c| !c.is_empty()) else {
        return fail("cancelled");
    };
    match signed_in(&app, &headers).await {
        Ok(Some(current)) if current.0.id == user_id => {}
        Ok(_) => return fail("expired"),
        Err(e) => {
            tracing::error!(error = %e, "session lookup failed");
            return fail("internal");
        }
    }
    let fetched = async {
        let token = app.threads.exchange(&code, &redirect_uri(&app)).await?;
        let me = app.threads.me(&token.access_token).await?;
        Ok::<_, PostError>((token, me))
    };
    let (token, me) = match fetched.await {
        Ok(found) => found,
        Err(PostError::Failed(why)) => {
            tracing::warn!(why, "threads connect failed");
            return fail("threads_failed");
        }
        Err(PostError::Reconnect) => {
            tracing::warn!("threads refused the new token");
            return fail("threads_failed");
        }
    };
    let now = Utc::now().fixed_offset();
    let new_id = Uuid::now_v7();
    let name = me.name.filter(|n| !n.trim().is_empty());
    let row = social_accounts::ActiveModel {
        id: Set(new_id),
        team_id: Set(team_id),
        provider: Set(PROVIDER.into()),
        external_id: Set(me.id),
        handle: Set(me.username.clone()),
        display_name: Set(name.or(me.username)),
        avatar_url: Set(me.threads_profile_picture_url),
        access_token: Set(app.cipher.encrypt(token.access_token.as_bytes())),
        token_expires_at: Set(Some(now + Duration::seconds(token.expires_in))),
        meta: Set(serde_json::json!({})),
        dm_access: Set(false),
        inbox_error_at: Set(None),
        created_by: Set(Some(user_id)),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    };
    // A reconnect keeps the row and takes the new token and profile.
    let refresh = [
        social_accounts::Column::Handle,
        social_accounts::Column::DisplayName,
        social_accounts::Column::AvatarUrl,
        social_accounts::Column::AccessToken,
        social_accounts::Column::TokenExpiresAt,
        social_accounts::Column::UpdatedAt,
    ];
    if let Err(e) = save_account(&app.db, (team_id, new_id), row, refresh).await {
        tracing::error!(error = %e, "threads account save failed");
        return fail("internal");
    }
    ([(header::SET_COOKIE, clear.clone())], back(&app, None)).into_response()
}

/// Extends every Threads token with 30 days or less left and not yet expired by another 60
/// days. A token changed meanwhile (a reconnect) is left as it is. Returns how many were
/// extended; a refused refresh is logged and the account waits for a reconnect. One instance at
/// a time (0 while another holds the lock).
pub async fn refresh_tokens(app: &AppState) -> Result<u64, DbErr> {
    let lock = app.db.begin().await?;
    let row = lock
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT pg_try_advisory_xact_lock($1) AS ok",
            [REFRESH_LOCK.into()],
        ))
        .await?;
    let locked: bool = match row {
        Some(r) => r.try_get("", "ok")?,
        None => false,
    };
    if !locked {
        return Ok(0);
    }
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT id, access_token FROM social_accounts WHERE provider = $1 \
         AND token_expires_at > now() AND token_expires_at <= now() + interval '30 days'",
        [PROVIDER.into()],
    );
    let mut done = 0;
    for row in app.db.query_all_raw(stmt).await? {
        let (id, sealed): (Uuid, Vec<u8>) =
            (row.try_get("", "id")?, row.try_get("", "access_token")?);
        let Some(old) = app
            .cipher
            .decrypt(&sealed)
            .and_then(|b| String::from_utf8(b).ok())
        else {
            tracing::warn!(account = %id, "threads token unreadable");
            continue;
        };
        let token = match app.threads.refresh(&old).await {
            Ok(t) => t,
            Err(PostError::Failed(why)) => {
                tracing::warn!(account = %id, why, "threads token refresh failed");
                continue;
            }
            Err(PostError::Reconnect) => {
                tracing::warn!(account = %id, "threads refused the token refresh");
                continue;
            }
        };
        let stmt = Statement::from_sql_and_values(
            DbBackend::Postgres,
            "UPDATE social_accounts SET access_token = $2, \
             token_expires_at = now() + make_interval(secs => $3), updated_at = now() \
             WHERE id = $1 AND access_token = $4",
            [
                id.into(),
                app.cipher.encrypt(token.access_token.as_bytes()).into(),
                (token.expires_in as f64).into(),
                sealed.into(),
            ],
        );
        done += app.db.execute_raw(stmt).await?.rows_affected();
    }
    lock.commit().await?;
    Ok(done)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/connections/threads/redirect", get(start))
        .route("/connect/threads/callback", get(callback))
}
