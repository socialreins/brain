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
    auth::{find_user, internal, start_session, user_json, Body, CurrentUser, FromWeb},
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
    // Both limits count every attempt, so one IP cannot spray many emails nor many IPs one email.
    let ip_ok = app.login_limiter.allow(ip);
    if !(app.login_email_limiter.hit(email.clone()) && ip_ok) {
        return rate_limited();
    }
    let user = match find_user(&app, &email).await {
        Ok(user) => user,
        Err(e) => return internal(e, "user lookup"),
    };
    let phc = user.as_ref().and_then(|u| u.password_hash.clone());
    let (Some(user), true) = (user, verify(body.password, phc).await) else {
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
/// a session from the last 10 minutes, so a stolen old cookie cannot add a password.
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
    if !app.login_email_limiter.hit(user.email.clone()) {
        return rate_limited();
    }
    let allowed = match &user.password_hash {
        Some(phc) => {
            let given = body.current_password.unwrap_or_default();
            verify(given, Some(phc.clone())).await
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
    let mut row = current.0.into_active_model();
    row.password_hash = Set(Some(phc));
    row.updated_at = Set(chrono::Utc::now().fixed_offset());
    match row.update(&app.db).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e, "password update"),
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/login", post(login))
        .route("/auth/password", put(set_password))
}
