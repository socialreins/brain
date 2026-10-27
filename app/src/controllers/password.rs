//! Password sign-in: `POST /auth/login` starts a session, `PUT /auth/password` sets or changes
//! the signed-in user's password. Both only answer the web app's origins.
use std::sync::LazyLock;

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{post, put},
    Router,
};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, IntoActiveModel};
use serde::Deserialize;
use socialreins_core::{email, password};

use super::{
    auth::{
        end_other_sessions, find_user, internal, start_session, user_json, Body, CurrentUser,
        FromWeb,
    },
    waitlist::error,
};
use crate::{rate_limit::ClientIp, state::AppState};

/// Verified when the account is missing or has no password, so those answers take as long as
/// a wrong password.
static DUMMY_HASH: LazyLock<String> = LazyLock::new(|| password::hash("not a real password"));

/// Argon2 is CPU bound, so it runs off the async workers.
async fn verify(candidate: String, phc: Option<String>) -> bool {
    tokio::task::spawn_blocking(move || {
        let ok = password::verify(&candidate, phc.as_deref().unwrap_or(&DUMMY_HASH));
        ok && phc.is_some()
    })
    .await
    .unwrap_or(false)
}

#[derive(Deserialize)]
struct Login {
    email: String,
    password: String,
}

/// Every password guess for `email`, from `/auth/login` or a signed-in confirm, draws from one
/// failure budget: a slot is reserved under the limiter's lock before Argon2 runs, so parallel
/// guesses cannot all slip past the cap, and a success clears the count. `None`: over budget.
pub(crate) async fn check_password(
    app: &AppState,
    email: &str,
    candidate: String,
    phc: Option<String>,
) -> Option<bool> {
    if !app.login_email_limiter.hit(email.to_owned()) {
        return None;
    }
    let ok = verify(candidate, phc).await;
    if ok {
        app.login_email_limiter.clear(&email.to_owned());
    }
    Some(ok)
}

fn rate_limited() -> Response {
    error(StatusCode::TOO_MANY_REQUESTS, "rate_limited").into_response()
}

/// `POST /auth/login`: the same `401 {"error":"invalid_credentials"}` for an unknown email, an
/// account without a password and a wrong password.
async fn login(
    _: FromWeb,
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    Body(body): Body<Login>,
) -> Response {
    let email = email::normalize(&body.email).unwrap_or_default();
    // Every attempt counts per IP; per email only failures stay counted (see `check_password`).
    if !app.login_limiter.allow(ip) {
        return rate_limited();
    }
    let user = match find_user(&app, &email).await {
        Ok(user) => user,
        Err(e) => return internal(e, "user lookup"),
    };
    let phc = user.as_ref().and_then(|u| u.password_hash.clone());
    let ok = match check_password(&app, &email, body.password, phc).await {
        Some(ok) => ok,
        None => return rate_limited(),
    };
    let (Some(user), true) = (user, ok) else {
        return error(StatusCode::UNAUTHORIZED, "invalid_credentials").into_response();
    };
    match start_session(&app, user.id).await {
        Ok(cookie) => ([(header::SET_COOKIE, cookie)], user_json(&user)).into_response(),
        Err(e) => internal(e, "session insert"),
    }
}

#[derive(Deserialize)]
struct SetPassword {
    password: String,
    current_password: Option<String>,
}

/// `PUT /auth/password`: changing a password needs the current one; setting the first one needs
/// a session from the last 10 minutes, so a stolen old cookie cannot add a password. Every other
/// session of the user ends.
async fn set_password(
    _: FromWeb,
    State(app): State<AppState>,
    current: CurrentUser,
    Body(body): Body<SetPassword>,
) -> Response {
    if !password::acceptable(&body.password) {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_password").into_response();
    }
    let user = &current.0;
    if !app.account_limiter.hit(user.id) {
        return rate_limited();
    }
    let allowed = match &user.password_hash {
        Some(phc) => {
            let given = body.current_password.unwrap_or_default();
            match check_password(&app, &user.email, given, Some(phc.clone())).await {
                Some(ok) => ok,
                None => return rate_limited(),
            }
        }
        None => current.recent(),
    };
    if !allowed {
        let code = if user.password_hash.is_some() {
            "invalid_current_password"
        } else {
            "reauth_required"
        };
        return error(StatusCode::FORBIDDEN, code).into_response();
    }
    let new = body.password;
    let Ok(phc) = tokio::task::spawn_blocking(move || password::hash(&new)).await else {
        return internal("hash task failed", "password hash");
    };
    let (user, session) = (current.0, current.1);
    let user_id = user.id;
    let mut row = user.into_active_model();
    row.password_hash = Set(Some(phc));
    row.updated_at = Set(chrono::Utc::now().fixed_offset());
    if let Err(e) = row.update(&app.db).await {
        return internal(e, "password update");
    }
    match end_other_sessions(&app, user_id, &session.token_hash).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e, "session revoke"),
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/login", post(login))
        .route("/auth/password", put(set_password))
}
