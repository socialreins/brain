//! Google sign-in. `GET /auth/google/start?next=` sends the browser to Google with a fresh
//! state, nonce and PKCE challenge; `GET /auth/google/callback` checks them, signs the Google
//! account in (by its linked `sub`, else its verified email, else a new account under the
//! magic link's sign-up rules) and lands on the web app. Failures land on
//! `{WEB_URL}/login?error=google_failed` (or `registration_disabled`), with no details.
//! `GET /auth/providers` says whether Google is set up.
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{AppendHeaders, IntoResponse, Redirect, Response},
    routing::get,
    Json, Router,
};
use chrono::{Duration, Utc};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DbBackend, DbErr, EntityTrait, QueryFilter,
    SqlErr, Statement,
};
use serde::Deserialize;
use serde_json::json;
use socialreins_core::{email, oauth, team, token};

use super::{
    auth::{cookie_value, named_cookie, refused, signed_in, start_session, user_for_sign_in},
    waitlist::error,
};
use crate::{connectors::google::Identity, models::users, rate_limit::ClientIp, state::AppState};

const STATE_COOKIE: &str = "sr_google";
const COOKIE_PATH: &str = "/auth/google";
const STATE_MINUTES: i64 = 10;
const DEFAULT_NEXT: &str = "/app";

fn redirect_uri(app: &AppState) -> String {
    format!("{}/auth/google/callback", app.brain_url)
}

fn not_configured() -> Response {
    error(StatusCode::NOT_FOUND, "not_found").into_response()
}

/// `GET /auth/providers`: the optional sign-in methods this install offers.
async fn providers(State(app): State<AppState>) -> Json<serde_json::Value> {
    let turnstile_site_key = app.turnstile.as_ref().map(|t| &t.site_key);
    Json(json!({ "google": app.google.is_some(), "turnstile_site_key": turnstile_site_key }))
}

#[derive(Deserialize)]
struct Start {
    next: Option<String>,
}

/// `GET /auth/google/start?next=/path`: `next` must be a path on the web app, else `/app`.
async fn start(
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    Query(q): Query<Start>,
) -> Response {
    let Some(google) = &app.google else {
        return not_configured();
    };
    if !app.login_limiter.allow(ip) {
        return refused(&app, "google_failed");
    }
    let next = q
        .next
        .as_deref()
        .and_then(oauth::safe_next)
        .unwrap_or(DEFAULT_NEXT);
    let (state, nonce, verifier) = (token::new(), token::new(), token::new());
    let saved = async {
        app.db
            .execute_unprepared("DELETE FROM oauth_states WHERE expires_at < now()")
            .await?;
        let stmt = Statement::from_sql_and_values(
            DbBackend::Postgres,
            "INSERT INTO oauth_states (state_hash, nonce, verifier, next, expires_at) \
             VALUES ($1, $2, $3, $4, $5)",
            [
                token::hash(&state).into(),
                nonce.clone().into(),
                verifier.clone().into(),
                next.to_owned().into(),
                (Utc::now() + Duration::minutes(STATE_MINUTES))
                    .fixed_offset()
                    .into(),
            ],
        );
        app.db.execute_raw(stmt).await.map(|_| ())
    };
    if let Err(e) = saved.await {
        tracing::error!(error = %e, "google state insert failed");
        return refused(&app, "google_failed");
    }
    let challenge = oauth::pkce_challenge(&verifier);
    let url = google.authorize_url(&redirect_uri(&app), &state, &nonce, &challenge);
    let max_age = Duration::minutes(STATE_MINUTES).num_seconds();
    let cookie = named_cookie(&app, STATE_COOKIE, COOKIE_PATH, &state, max_age);
    ([(header::SET_COOKIE, cookie)], Redirect::to(&url)).into_response()
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
}

/// The attempt stored for `state`, deleted as it is read, so it works once.
async fn take_state(
    app: &AppState,
    state: &str,
) -> Result<Option<(String, String, String)>, DbErr> {
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "DELETE FROM oauth_states WHERE state_hash = $1 RETURNING nonce, verifier, next, \
         expires_at > now() AS live",
        [token::hash(state).into()],
    );
    let Some(row) = app.db.query_one_raw(stmt).await? else {
        return Ok(None);
    };
    let live: bool = row.try_get("", "live")?;
    Ok(live
        .then(|| {
            Some((
                row.try_get("", "nonce").ok()?,
                row.try_get("", "verifier").ok()?,
                row.try_get("", "next").ok()?,
            ))
        })
        .flatten())
}

enum Outcome {
    User(users::Model, bool),
    /// Sign-up is closed to this email.
    Closed,
    /// The email's account is linked to another Google account.
    Conflict,
}

/// The account a Google identity signs in to: its linked `sub`; else, when Google vouches for
/// the mailbox (`Identity::owns_email`), the account of that email (linking the `sub`) or a new
/// one under the magic link's sign-up rules. Any other address links only to the account
/// signed in on this browser in the last 10 minutes with that same email (e.g. by magic link).
async fn account_for(
    app: &AppState,
    id: &Identity,
    current: Option<&super::auth::CurrentUser>,
) -> Result<Outcome, DbErr> {
    let linked = users::Entity::find()
        .filter(users::Column::GoogleSub.eq(&id.sub))
        .one(&app.db)
        .await?;
    if let Some(user) = linked {
        return Ok(Outcome::User(user, false));
    }
    let Some(email) = email::normalize(&id.email) else {
        return Ok(Outcome::Conflict);
    };
    let (user, created) = if id.owns_email() {
        match user_for_sign_in(app, &email).await? {
            Some(found) => found,
            None => return Ok(Outcome::Closed),
        }
    } else {
        match current {
            Some(c) if c.recent() && c.0.email == email => (c.0.clone(), false),
            _ => return Ok(Outcome::Conflict),
        }
    };
    match &user.google_sub {
        Some(sub) if *sub == id.sub => {}
        Some(_) => return Ok(Outcome::Conflict),
        None => {
            let link = Statement::from_sql_and_values(
                DbBackend::Postgres,
                "UPDATE users SET google_sub = $2, updated_at = now() \
                 WHERE id = $1 AND google_sub IS NULL",
                [user.id.into(), id.sub.clone().into()],
            );
            match app.db.execute_raw(link).await {
                Ok(r) if r.rows_affected() == 1 => {}
                // Another sign-in linked a different Google account meanwhile.
                Ok(_) => return Ok(Outcome::Conflict),
                // This `sub` was linked to another account meanwhile.
                Err(e) if matches!(e.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) => {
                    return Ok(Outcome::Conflict)
                }
                Err(e) => return Err(e),
            }
        }
    }
    let name = id.name.as_deref().and_then(team::name);
    if let (true, Some(name)) = (created, name) {
        let row = users::ActiveModel {
            id: Set(user.id),
            name: Set(name),
            ..Default::default()
        };
        users::Entity::update(row).exec(&app.db).await?;
    }
    Ok(Outcome::User(user, created))
}

/// `GET /auth/google/callback`: Google sends the browser here with `code` and `state`.
async fn callback(
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Query(q): Query<Callback>,
) -> Response {
    let Some(google) = &app.google else {
        return not_configured();
    };
    let clear = named_cookie(&app, STATE_COOKIE, COOKIE_PATH, "", 0);
    let fail =
        |code: &str| ([(header::SET_COOKIE, clear.clone())], refused(&app, code)).into_response();
    if !app.login_limiter.allow(ip) {
        return fail("google_failed");
    }
    let (Some(code), Some(state)) = (q.code, q.state) else {
        return fail("google_failed");
    };
    // The browser that started the sign-in must be the one finishing it (login CSRF).
    let from_this_browser =
        cookie_value(&headers, STATE_COOKIE).is_some_and(|c| token::hash(c) == token::hash(&state));
    if !from_this_browser {
        return fail("google_failed");
    }
    let (nonce, verifier, next) = match take_state(&app, &state).await {
        Ok(Some(found)) => found,
        Ok(None) => return fail("google_failed"),
        Err(e) => {
            tracing::error!(error = %e, "google state lookup failed");
            return fail("google_failed");
        }
    };
    let Some(identity) = google
        .exchange(&code, &verifier, &redirect_uri(&app), &nonce)
        .await
    else {
        return fail("google_failed");
    };
    let current = match signed_in(&app, &headers).await {
        Ok(current) => current,
        Err(e) => {
            tracing::error!(error = %e, "session lookup failed");
            return fail("google_failed");
        }
    };
    let (user, created) = match account_for(&app, &identity, current.as_ref()).await {
        Ok(Outcome::User(user, created)) => (user, created),
        Ok(Outcome::Closed) => return fail("registration_disabled"),
        Ok(Outcome::Conflict) => return fail("google_failed"),
        Err(e) => {
            tracing::error!(error = %e, "google account lookup failed");
            return fail("google_failed");
        }
    };
    let session = match start_session(&app, user.id).await {
        Ok(cookie) => cookie,
        Err(e) => {
            tracing::error!(error = %e, "session insert failed");
            return fail("google_failed");
        }
    };
    // `next` passed `safe_next` before it was stored; checked again on the way out.
    let next = oauth::safe_next(&next).unwrap_or(DEFAULT_NEXT);
    let to = match (created, next) {
        (false, next) => format!("{}{next}", app.web_url),
        (true, DEFAULT_NEXT) => format!("{}/welcome", app.web_url),
        // A new account keeps where it was going, e.g. an invitation.
        (true, next) => {
            reqwest::Url::parse_with_params(&format!("{}/welcome", app.web_url), [("next", next)])
                .map_or_else(|_| format!("{}/welcome", app.web_url), String::from)
        }
    };
    (
        AppendHeaders([(header::SET_COOKIE, session), (header::SET_COOKIE, clear)]),
        Redirect::to(&to),
    )
        .into_response()
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/providers", get(providers))
        .route("/auth/google/start", get(start))
        .route("/auth/google/callback", get(callback))
}
