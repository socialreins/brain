//! Sign-in by magic link and server-side sessions.
//!
//! `POST /auth/link` emails a link, `GET /auth/magic/{token}` redeems it once and sets the
//! session cookie, `GET /auth/me` returns the signed-in user, `POST /auth/logout` ends it.
use axum::{
    extract::{FromRequest, FromRequestParts, Path, Request, State},
    http::{header, request::Parts, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{Duration, Utc};
use sea_orm::{ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};
use serde::Deserialize;
use serde_json::json;
use socialreins_core::{email, token};

use super::waitlist::{error, rejected, Reply};
use crate::{
    models::{magic_links, sessions, users},
    rate_limit::ClientIp,
    state::AppState,
};

pub const SESSION_COOKIE: &str = "sr_session";
const LINK_MINUTES: i64 = 15;
const SESSION_DAYS: i64 = 30;

#[derive(Deserialize)]
struct LinkRequest {
    email: String,
}

fn internal(e: impl std::fmt::Display, what: &str) -> Response {
    tracing::error!(error = %e, what, "auth query failed");
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal").into_response()
}

/// Same 202 for a new email, a known one, and one refused by `REGISTRATION_DISABLED`, so the
/// reply never says whether an account exists. The mail goes out in the background for the same
/// reason: its timing would tell.
async fn request_link(
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    req: Request,
) -> Response {
    if !app.magic_link_limiter.allow(ip) {
        return error(StatusCode::TOO_MANY_REQUESTS, "rate_limited").into_response();
    }
    let body = match Json::<LinkRequest>::from_request(req, &()).await {
        Ok(Json(body)) => body,
        Err(r) => return rejected(&r).into_response(),
    };
    let Some(email) = email::normalize(&body.email) else {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_email").into_response();
    };
    let sent = (StatusCode::ACCEPTED, Json(json!({"status": "sent"}))).into_response();
    if app.registration_disabled {
        match find_user(&app, &email).await {
            Ok(Some(_)) => {}
            Ok(None) => return sent,
            Err(e) => return internal(e, "user lookup"),
        }
    }
    let now = Utc::now().fixed_offset();
    let expired = magic_links::Entity::delete_many()
        .filter(magic_links::Column::ExpiresAt.lt(now))
        .exec(&app.db)
        .await;
    if let Err(e) = expired {
        return internal(e, "magic link cleanup");
    }
    let secret = token::new();
    let link = magic_links::ActiveModel {
        token_hash: Set(token::hash(&secret)),
        email: Set(email.clone()),
        expires_at: Set(now + Duration::minutes(LINK_MINUTES)),
        ..Default::default()
    };
    if let Err(e) = magic_links::Entity::insert(link)
        .exec_without_returning(&app.db)
        .await
    {
        return internal(e, "magic link insert");
    }
    let url = format!("{}/auth/magic/{secret}", app.brain_url);
    let text = format!(
        "Open this link to sign in to SocialReins:\n\n{url}\n\nIt works once and expires in \
         {LINK_MINUTES} minutes. If you did not ask for it, ignore this email.\n"
    );
    tokio::spawn(async move {
        if let Err(e) = app.mailer.send(&email, "Your sign-in link", text).await {
            tracing::error!(error = %e, "magic link mail failed");
        }
    });
    sent
}

async fn find_user(app: &AppState, email: &str) -> Result<Option<users::Model>, sea_orm::DbErr> {
    users::Entity::find()
        .filter(users::Column::Email.eq(email))
        .one(&app.db)
        .await
}

/// Lands on `{WEB_URL}/login?error=...` when the link cannot be used.
fn refused(app: &AppState, code: &str) -> Response {
    Redirect::to(&format!("{}/login?error={code}", app.web_url)).into_response()
}

async fn redeem(
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    Path(secret): Path<String>,
) -> Response {
    if !app.magic_link_limiter.allow(ip) {
        return error(StatusCode::TOO_MANY_REQUESTS, "rate_limited").into_response();
    }
    let hash = token::hash(&secret);
    let link = match magic_links::Entity::find_by_id(hash.clone())
        .one(&app.db)
        .await
    {
        Ok(Some(link)) => link,
        Ok(None) => return refused(&app, "invalid_link"),
        Err(e) => return internal(e, "magic link lookup"),
    };
    // Whoever deletes the row owns the link, so two concurrent opens cannot both sign in.
    match magic_links::Entity::delete_by_id(hash).exec(&app.db).await {
        Ok(r) if r.rows_affected == 1 => {}
        Ok(_) => return refused(&app, "invalid_link"),
        Err(e) => return internal(e, "magic link delete"),
    }
    if link.expires_at < Utc::now() {
        return refused(&app, "invalid_link");
    }
    let user = match find_user(&app, &link.email).await {
        Ok(Some(user)) => user,
        Ok(None) if app.registration_disabled => return refused(&app, "registration_disabled"),
        Ok(None) => match create_user(&app, &link.email).await {
            Ok(user) => user,
            Err(e) => return internal(e, "user insert"),
        },
        Err(e) => return internal(e, "user lookup"),
    };
    let secret = token::new();
    let now = Utc::now().fixed_offset();
    let expired = sessions::Entity::delete_many()
        .filter(sessions::Column::ExpiresAt.lt(now))
        .exec(&app.db)
        .await;
    if let Err(e) = expired {
        tracing::warn!(error = %e, "expired session cleanup failed");
    }
    let session = sessions::ActiveModel {
        token_hash: Set(token::hash(&secret)),
        user_id: Set(user.id),
        expires_at: Set(now + Duration::days(SESSION_DAYS)),
        ..Default::default()
    };
    if let Err(e) = sessions::Entity::insert(session)
        .exec_without_returning(&app.db)
        .await
    {
        return internal(e, "session insert");
    }
    let cookie = cookie(&app, &secret, Duration::days(SESSION_DAYS).num_seconds());
    ([(header::SET_COOKIE, cookie)], Redirect::to(&app.web_url)).into_response()
}

async fn create_user(app: &AppState, email: &str) -> Result<users::Model, sea_orm::DbErr> {
    let name = email.split('@').next().unwrap_or_default().to_owned();
    let row = users::ActiveModel {
        id: Set(uuid::Uuid::now_v7()),
        email: Set(email.to_owned()),
        name: Set(name),
        ..Default::default()
    };
    // Two links for one new email redeemed at once: the second insert is a no-op.
    users::Entity::insert(row)
        .on_conflict_do_nothing_on([users::Column::Email])
        .exec_without_returning(&app.db)
        .await?;
    find_user(app, email)
        .await?
        .ok_or_else(|| sea_orm::DbErr::RecordNotFound(email.into()))
}

fn cookie(app: &AppState, value: &str, max_age: i64) -> String {
    let secure = if app.web_url.starts_with("http://") {
        ""
    } else {
        "; Secure"
    };
    format!("{SESSION_COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}")
}

fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(name, _)| *name == SESSION_COOKIE)
        .map(|(_, value)| value)
}

/// The signed-in user, from an unexpired session; `401 {"error":"unauthorized"}` otherwise.
pub struct CurrentUser(pub users::Model);

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = Reply;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, Reply> {
        let unauthorized = || error(StatusCode::UNAUTHORIZED, "unauthorized");
        let internal = |e: sea_orm::DbErr| {
            tracing::error!(error = %e, "session lookup failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal")
        };
        let secret = session_token(&parts.headers).ok_or_else(unauthorized)?;
        let session = sessions::Entity::find_by_id(token::hash(secret))
            .filter(sessions::Column::ExpiresAt.gt(Utc::now().fixed_offset()))
            .one(&app.db)
            .await
            .map_err(internal)?
            .ok_or_else(unauthorized)?;
        let user = users::Entity::find_by_id(session.user_id)
            .one(&app.db)
            .await
            .map_err(internal)?
            .ok_or_else(unauthorized)?;
        Ok(Self(user))
    }
}

async fn me(CurrentUser(user): CurrentUser) -> Reply {
    let body = json!({
        "id": user.id,
        "email": user.email,
        "name": user.name,
        "username": user.username,
    });
    (StatusCode::OK, Json(body))
}

async fn logout(State(app): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(secret) = session_token(&headers) {
        let deleted = sessions::Entity::delete_by_id(token::hash(secret))
            .exec(&app.db)
            .await;
        if let Err(e) = deleted {
            return internal(e, "session delete");
        }
    }
    let clear = cookie(&app, "", 0);
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, clear)]).into_response()
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/link", post(request_link))
        .route("/auth/magic/{token}", get(redeem))
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
}
