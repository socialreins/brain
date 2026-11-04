//! What every OAuth connect (Threads, Meta, LinkedIn, YouTube, X) shares. `start` sends an owner or admin of the
//! current team to the platform with a fresh state, stored server-side with that team and user
//! for 10 minutes, its hash only, while the browser holds the state in a cookie scoped to
//! `/connect/<provider>`. `finish` checks the cookie against the state in constant time, uses the
//! state once, and checks the same user is signed in and still manages that team. Both land on
//! `{WEB_URL}/app/connections`: `?connected=<provider>`, or
//! `?connect_error=<code>&provider=<provider>` with no details.
use axum::{
    http::{header, HeaderMap},
    response::{IntoResponse, Redirect, Response},
};
use chrono::{Duration, Utc};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};
use serde::Deserialize;
use socialreins_core::token;
use uuid::Uuid;

use super::{
    auth::{cookie_value, named_cookie, signed_in},
    teams::InTeam,
    waitlist::Reply,
};
use crate::state::AppState;

const STATE_MINUTES: i64 = 10;

fn cookie_name(provider: &str) -> String {
    format!("sr_{provider}")
}

fn cookie_path(provider: &str) -> String {
    format!("/connect/{provider}")
}

/// The callback registered with the platform; the path must not change.
pub(crate) fn redirect_uri(app: &AppState, provider: &str) -> String {
    format!("{}/connect/{provider}/callback", app.brain_url)
}

/// Back to the web app's connections page with `params`.
pub(crate) fn back(app: &AppState, params: &[(&str, &str)]) -> Response {
    let page = format!("{}/app/connections", app.web_url);
    let to = reqwest::Url::parse_with_params(&page, params).map_or(page, String::from);
    Redirect::to(&to).into_response()
}

/// Back to the web app saying the connect of `provider` failed with `code`.
pub(crate) fn back_failed(app: &AppState, provider: &str, code: &str) -> Response {
    back(app, &[("connect_error", code), ("provider", provider)])
}

/// `GET /connections/<provider>/redirect`: owners and admins of the current team only.
/// `authorize` makes the platform's URL from the redirect URI and the state.
pub(crate) async fn start(
    app: &AppState,
    t: Result<InTeam, Reply>,
    how: (&str, bool),
    authorize: impl FnOnce(&str, &str) -> Option<String>,
) -> Response {
    start_with(app, t, how, "", authorize).await
}

/// `start` for a provider that uses PKCE: `verifier` (sealed by the caller, never the plain
/// verifier) is kept with the state and comes back from `finish_with`.
pub(crate) async fn start_with(
    app: &AppState,
    t: Result<InTeam, Reply>,
    (provider, configured): (&str, bool),
    verifier: &str,
    authorize: impl FnOnce(&str, &str) -> Option<String>,
) -> Response {
    let fail = |code| back_failed(app, provider, code);
    if !configured {
        return fail("not_configured");
    }
    let Ok(t) = t else {
        return fail("unauthorized");
    };
    if !t.role.manages() {
        return fail("forbidden");
    }
    if !app.connect_limiter.hit(t.current.0.id) {
        return fail("rate_limited");
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
             VALUES ($1, '', $6, '', $2, $3, $4, $5)",
            [
                token::hash(&state).into(),
                (Utc::now() + Duration::minutes(STATE_MINUTES))
                    .fixed_offset()
                    .into(),
                provider.into(),
                t.team.id.into(),
                t.current.0.id.into(),
                verifier.into(),
            ],
        );
        app.db.execute_raw(stmt).await.map(|_| ())
    };
    if let Err(e) = saved.await {
        tracing::error!(error = %e, provider, "oauth state insert failed");
        return fail("internal");
    }
    let Some(url) = authorize(&redirect_uri(app, provider), &state) else {
        return fail("not_configured");
    };
    let max_age = Duration::minutes(STATE_MINUTES).num_seconds();
    let cookie = named_cookie(
        app,
        &cookie_name(provider),
        &cookie_path(provider),
        &state,
        max_age,
    );
    ([(header::SET_COOKIE, cookie)], Redirect::to(&url)).into_response()
}

#[derive(Deserialize)]
pub(crate) struct Callback {
    code: Option<String>,
    state: Option<String>,
}

/// The attempt stored for `state`, deleted as it is read so it works once: `(team, user,
/// verifier)` when it is live and the user still manages the team.
async fn take_state(
    app: &AppState,
    provider: &str,
    state: &str,
) -> Result<Option<(Uuid, Uuid, String)>, DbErr> {
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "DELETE FROM oauth_states s WHERE state_hash = $1 AND provider = $2 \
         RETURNING team_id, user_id, verifier, expires_at > now() AND EXISTS ( \
           SELECT 1 FROM team_user m WHERE m.team_id = s.team_id AND m.user_id = s.user_id \
           AND m.role IN ('owner', 'admin')) AS ok",
        [token::hash(state).into(), provider.into()],
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
        row.try_get("", "verifier")?,
    )))
}

/// Lands on the web app with the state cookie cleared: `connected` on `None`, else the code.
pub(crate) fn landed(app: &AppState, provider: &str, params: &[(&str, &str)]) -> Response {
    let clear = named_cookie(app, &cookie_name(provider), &cookie_path(provider), "", 0);
    ([(header::SET_COOKIE, clear)], back(app, params)).into_response()
}

/// `GET /connect/<provider>/callback`, the checks before any platform call: `(team, user,
/// code)` of a live attempt this browser started, or the landing that says why not.
pub(crate) async fn finish(
    app: &AppState,
    headers: &HeaderMap,
    q: Callback,
    how: (&str, bool),
) -> Result<(Uuid, Uuid, String), Box<Response>> {
    let (team, user, code, _) = finish_with(app, headers, q, how).await?;
    Ok((team, user, code))
}

/// `finish` for a PKCE provider: also the sealed verifier `start_with` stored.
pub(crate) async fn finish_with(
    app: &AppState,
    headers: &HeaderMap,
    q: Callback,
    (provider, configured): (&str, bool),
) -> Result<(Uuid, Uuid, String, String), Box<Response>> {
    let fail = |code| {
        Box::new(landed(
            app,
            provider,
            &[("connect_error", code), ("provider", provider)],
        ))
    };
    if !configured {
        return Err(fail("not_configured"));
    }
    let Some(state) = q.state else {
        return Err(fail("expired"));
    };
    // The browser that started the connect must be the one finishing it.
    let from_this_browser =
        cookie_value(headers, &cookie_name(provider)).is_some_and(|c| token::same(c, &state));
    if !from_this_browser {
        return Err(fail("expired"));
    }
    let (team_id, user_id, verifier) = match take_state(app, provider, &state).await {
        Ok(Some(found)) => found,
        Ok(None) => return Err(fail("expired")),
        Err(e) => {
            tracing::error!(error = %e, provider, "oauth state lookup failed");
            return Err(fail("internal"));
        }
    };
    let Some(code) = q.code.filter(|c| !c.is_empty()) else {
        return Err(fail("cancelled"));
    };
    match signed_in(app, headers).await {
        Ok(Some(current)) if current.0.id == user_id => Ok((team_id, user_id, code, verifier)),
        Ok(_) => Err(fail("expired")),
        Err(e) => {
            tracing::error!(error = %e, "session lookup failed");
            Err(fail("internal"))
        }
    }
}
