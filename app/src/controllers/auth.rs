//! Sign-in by magic link and server-side sessions.
//!
//! `POST /auth/link` emails a link, `GET /auth/magic/{token}` shows a sign-in button that posts
//! it to `POST /auth/magic`, which uses it once and sets the session cookie, `GET /auth/me` returns the signed-in user, `POST /auth/logout` ends it.
//! Every sign-in gives a user without a team a personal one (see `teams::ensure_team`).
use axum::{
    extract::{Form, FromRequest, FromRequestParts, Path, Request, State},
    http::{header, request::Parts, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{Duration, Utc};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DbBackend, DbErr, EntityTrait, PaginatorTrait,
    QueryFilter, Statement, TransactionTrait,
};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::json;
use socialreins_core::{email, token};

use super::{
    teams,
    waitlist::{error, rejected, Reply},
};
use crate::{
    config::origin,
    models::{magic_links, sessions, users},
    rate_limit::ClientIp,
    state::AppState,
};

pub const SESSION_COOKIE: &str = "sr_session";
const LINK_MINUTES: i64 = 15;
const SESSION_DAYS: i64 = 30;
/// Unexpired links one email can hold; past it, `POST /auth/link` sends nothing.
const MAX_LIVE_LINKS: u64 = 3;

#[derive(Deserialize)]
struct LinkRequest {
    email: String,
}

pub(crate) fn internal(e: impl std::fmt::Display, what: &str) -> Response {
    tracing::error!(error = %e, what, "auth query failed");
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal").into_response()
}

/// Same 202, at the same speed, for a new email, a known one, and one refused by
/// `REGISTRATION_DISABLED`: the lookup, insert and mail all run after the reply, so neither the
/// body nor the timing says whether an account exists.
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
    tokio::spawn(async move {
        if let Err(e) = send_link(&app, email).await {
            tracing::error!(error = %e, "magic link not sent");
        }
    });
    (StatusCode::ACCEPTED, Json(json!({"status": "sent"}))).into_response()
}

/// Errors carry the failure only, never the email or the token.
async fn send_link(app: &AppState, email: String) -> Result<(), String> {
    if !may_sign_up(app, &email)
        .await
        .map_err(|e| format!("sign-up check: {e}"))?
        && find_user(app, &email)
            .await
            .map_err(|e| format!("user lookup: {e}"))?
            .is_none()
    {
        return Ok(());
    }
    let now = Utc::now().fixed_offset();
    magic_links::Entity::delete_many()
        .filter(magic_links::Column::ExpiresAt.lt(now))
        .exec(&app.db)
        .await
        .map_err(|e| format!("magic link cleanup: {e}"))?;
    let Some(secret) = insert_capped_link(app, &email)
        .await
        .map_err(|e| format!("magic link insert: {e}"))?
    else {
        return Ok(());
    };
    let url = format!("{}/auth/magic/{secret}", app.brain_url);
    let text = format!(
        "Open this link to sign in to SocialReins:\n\n{url}\n\nIt works once and expires in \
         {LINK_MINUTES} minutes. If you did not ask for it, ignore this email.\n"
    );
    app.mailer
        .send(&email, "Your sign-in link", text)
        .await
        .map_err(|e| format!("mail: {e}"))
}

/// Stores a new link unless `email` already holds `MAX_LIVE_LINKS` unexpired ones, which caps
/// mail to one inbox no matter how many IPs ask. The count and insert run under a per-email
/// advisory lock, so concurrent requests cannot both pass the check. Returns the new token.
async fn insert_capped_link(app: &AppState, email: &str) -> Result<Option<String>, DbErr> {
    let txn = app.db.begin().await?;
    let lock = "SELECT pg_advisory_xact_lock(hashtext($1))";
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        lock,
        [email.into()],
    ))
    .await?;
    let now = Utc::now().fixed_offset();
    let live = magic_links::Entity::find()
        .filter(magic_links::Column::Email.eq(email))
        .filter(magic_links::Column::ExpiresAt.gt(now))
        .count(&txn)
        .await?;
    if live >= MAX_LIVE_LINKS {
        return Ok(None);
    }
    let secret = token::new();
    let link = magic_links::ActiveModel {
        token_hash: Set(token::hash(&secret)),
        email: Set(email.to_owned()),
        expires_at: Set(now + Duration::minutes(LINK_MINUTES)),
        ..Default::default()
    };
    magic_links::Entity::insert(link)
        .exec_without_returning(&txn)
        .await?;
    txn.commit().await?;
    Ok(Some(secret))
}

pub(crate) async fn find_user(
    app: &AppState,
    email: &str,
) -> Result<Option<users::Model>, sea_orm::DbErr> {
    users::Entity::find()
        .filter(users::Column::Email.eq(email))
        .one(&app.db)
        .await
}

/// Whether a new account for `email` may be created.
enum SignUp {
    Allowed,
    /// Closed registration without `OWNER_EMAIL`: only if no account exists yet.
    FirstOnly,
    Refused,
}

/// Open registration admits anyone. Closed (`REGISTRATION_DISABLED`) admits `OWNER_EMAIL` and
/// invited emails, or, with no `OWNER_EMAIL`, whoever signs up first.
async fn sign_up(app: &AppState, email: &str) -> Result<SignUp, DbErr> {
    if !app.registration_disabled
        || app.owner_email.as_deref() == Some(email)
        || teams::invited(&app.db, email).await?
    {
        return Ok(SignUp::Allowed);
    }
    Ok(match app.owner_email {
        None => SignUp::FirstOnly,
        Some(_) => SignUp::Refused,
    })
}

/// `sign_up` as a yes or no, without the lock `create_first_user` takes: for deciding whether
/// to send a link at all.
async fn may_sign_up(app: &AppState, email: &str) -> Result<bool, DbErr> {
    Ok(match sign_up(app, email).await? {
        SignUp::Allowed => true,
        SignUp::FirstOnly => users::Entity::find().one(&app.db).await?.is_none(),
        SignUp::Refused => false,
    })
}

/// `SignUp::FirstOnly`: creates the account only if none exists. The advisory lock makes
/// concurrent first sign-ups take turns, so exactly one sees an empty `users` table.
async fn create_first_user(app: &AppState, email: &str) -> Result<Option<users::Model>, DbErr> {
    const FIRST_ACCOUNT_LOCK: i64 = 0x0053_5246_4952_5354; // "SRFIRST"
    let txn = app.db.begin().await?;
    txn.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_advisory_xact_lock($1)",
        [FIRST_ACCOUNT_LOCK.into()],
    ))
    .await?;
    if users::Entity::find().one(&txn).await?.is_some() {
        return Ok(None);
    }
    let user = create_user(&txn, email).await?;
    txn.commit().await?;
    Ok(Some(user))
}

/// Lands on `{WEB_URL}/login?error=...` when the link cannot be used.
fn refused(app: &AppState, code: &str) -> Response {
    Redirect::to(&format!("{}/login?error={code}", app.web_url)).into_response()
}

/// `GET /auth/magic/{token}`: a page whose button posts the token back. Opening the link does
/// not use it up, so mail scanners that prefetch links cannot burn it, and a link alone cannot
/// sign a browser in without a click.
async fn confirm_page(State(app): State<AppState>, Path(secret): Path<String>) -> Response {
    // Only our own 64-hex tokens reach the HTML, so nothing needs escaping.
    if secret.len() != 64 || !secret.bytes().all(|b| b.is_ascii_hexdigit()) {
        return refused(&app, "invalid_link");
    }
    let page = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width\"><title>Sign in</title></head>\
         <body><form method=\"post\" action=\"/auth/magic\">\
         <input type=\"hidden\" name=\"token\" value=\"{secret}\">\
         <button type=\"submit\">Sign in to SocialReins</button></form></body></html>"
    );
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            // Not no-referrer: that makes the browser send `Origin: null` on the form post.
            (header::REFERRER_POLICY, "same-origin"),
        ],
        Html(page),
    )
        .into_response()
}

#[derive(Deserialize)]
struct Redeem {
    token: String,
}

/// `POST /auth/magic` with form field `token`: uses the link once and starts the session.
/// Only the button on brain's own page may post here, so another site cannot sign a browser
/// in to an account of its choosing (login CSRF).
async fn redeem(
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Form(Redeem { token: secret }): Form<Redeem>,
) -> Response {
    if !app.magic_link_limiter.allow(ip) {
        return error(StatusCode::TOO_MANY_REQUESTS, "rate_limited").into_response();
    }
    if !from_brain(&app, &headers) {
        return error(StatusCode::FORBIDDEN, "forbidden_origin").into_response();
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
        Ok(None) => {
            let created = match sign_up(&app, &link.email).await {
                Ok(SignUp::Allowed) => create_user(&app.db, &link.email).await.map(Some),
                Ok(SignUp::FirstOnly) => create_first_user(&app, &link.email).await,
                Ok(SignUp::Refused) => Ok(None),
                Err(e) => return internal(e, "sign-up check"),
            };
            match created {
                Ok(Some(user)) => user,
                Ok(None) => return refused(&app, "registration_disabled"),
                Err(e) => return internal(e, "user insert"),
            }
        }
        Err(e) => return internal(e, "user lookup"),
    };
    match start_session(&app, user.id).await {
        Ok(cookie) => ([(header::SET_COOKIE, cookie)], Redirect::to(&app.web_url)).into_response(),
        Err(e) => internal(e, "session insert"),
    }
}

/// Stores a new 30-day session for `user_id` and returns its `Set-Cookie` value. Every sign-in
/// method ends here, so this is where a user without a team gets their personal one.
pub(crate) async fn start_session(app: &AppState, user_id: uuid::Uuid) -> Result<String, DbErr> {
    teams::ensure_team(&app.db, user_id).await?;
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
        user_id: Set(user_id),
        expires_at: Set(now + Duration::days(SESSION_DAYS)),
        ..Default::default()
    };
    sessions::Entity::insert(session)
        .exec_without_returning(&app.db)
        .await?;
    Ok(cookie(
        app,
        &secret,
        Duration::days(SESSION_DAYS).num_seconds(),
    ))
}

/// Deletes every session of `user_id` except the one hashed `keep`, after a credential change.
pub(crate) async fn end_other_sessions(
    app: &AppState,
    user_id: uuid::Uuid,
    keep: &str,
) -> Result<(), DbErr> {
    sessions::Entity::delete_many()
        .filter(sessions::Column::UserId.eq(user_id))
        .filter(sessions::Column::TokenHash.ne(keep))
        .exec(&app.db)
        .await
        .map(|_| ())
}

/// `Origin`, or the origin of `Referer` when a browser sent no `Origin`.
fn sent_origin(headers: &HeaderMap) -> Option<&str> {
    let value = |name| headers.get(name).and_then(|v| v.to_str().ok());
    value(header::ORIGIN).or_else(|| value(header::REFERER).map(origin))
}

/// The request came from BRAIN_URL's origin.
fn from_brain(app: &AppState, headers: &HeaderMap) -> bool {
    sent_origin(headers) == Some(origin(&app.brain_url))
}

/// Guard for state-changing calls the web app makes: like the check on `POST /auth/magic`, but
/// the origin must be one of `CORS_ORIGINS`. Otherwise `403 {"error":"forbidden_origin"}`, so
/// another site cannot drive a signed-in browser (CSRF) or sign it in (login CSRF).
pub struct FromWeb;

impl FromRequestParts<AppState> for FromWeb {
    type Rejection = Reply;

    async fn from_request_parts(parts: &mut Parts, app: &AppState) -> Result<Self, Reply> {
        let sent = sent_origin(&parts.headers);
        if sent.is_some_and(|o| app.web_origins.iter().any(|w| w == o)) {
            Ok(Self)
        } else {
            Err(error(StatusCode::FORBIDDEN, "forbidden_origin"))
        }
    }
}

/// A JSON body, refused with the same JSON errors as `/v1`.
pub struct Body<T>(pub T);

impl<T: DeserializeOwned, S: Send + Sync> FromRequest<S> for Body<T> {
    type Rejection = Reply;

    async fn from_request(req: Request, state: &S) -> Result<Self, Reply> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(body)) => Ok(Self(body)),
            Err(r) => Err(rejected(&r)),
        }
    }
}

async fn create_user(db: &impl ConnectionTrait, email: &str) -> Result<users::Model, DbErr> {
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
        .exec_without_returning(db)
        .await?;
    users::Entity::find()
        .filter(users::Column::Email.eq(email))
        .one(db)
        .await?
        .ok_or_else(|| sea_orm::DbErr::RecordNotFound("user vanished after insert".into()))
}

fn cookie(app: &AppState, value: &str, max_age: i64) -> String {
    let secure = if app.brain_url.starts_with("http://") {
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

/// The signed-in user and their session, from an unexpired session cookie;
/// `401 {"error":"unauthorized"}` otherwise.
pub struct CurrentUser(pub users::Model, pub sessions::Model);

/// How long a session counts as a fresh sign-in, for changes a stolen old cookie must not make.
const RECENT_AUTH_MINUTES: i64 = 10;

impl CurrentUser {
    /// The session started within the last `RECENT_AUTH_MINUTES`.
    pub fn recent(&self) -> bool {
        self.1.created_at > Utc::now() - Duration::minutes(RECENT_AUTH_MINUTES)
    }
}

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
        Ok(Self(user, session))
    }
}

/// The public fields of `user`, as `GET /auth/me` returns them.
pub(crate) fn user_json(user: &users::Model) -> Json<serde_json::Value> {
    Json(json!({
        "id": user.id,
        "email": user.email,
        "name": user.name,
        "username": user.username,
        "has_password": user.password_hash.is_some(),
    }))
}

/// The user, plus `current_team` `{id, name, role}`.
async fn me(State(app): State<AppState>, CurrentUser(user, _): CurrentUser) -> Response {
    let (team, role) = match teams::current_team(&app.db, &user).await {
        Ok(found) => found,
        Err(e) => return internal(e, "current team lookup"),
    };
    let Json(mut body) = user_json(&user);
    body["current_team"] = json!({"id": team.id, "name": team.name, "role": role.as_str()});
    Json(body).into_response()
}

/// `POST /auth/logout`: only the web app's origins, so another site cannot sign a user out.
async fn logout(_: FromWeb, State(app): State<AppState>, headers: HeaderMap) -> Response {
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
        .route("/auth/magic", post(redeem))
        .route("/auth/magic/{token}", get(confirm_page))
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
}
