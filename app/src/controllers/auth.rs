//! Sign-in by magic link and server-side sessions.
//!
//! `POST /auth/link` emails a link, `GET /auth/magic/{token}` shows a sign-in button that posts
//! it to `POST /auth/magic`, which uses it once and sets the session cookie, `GET /auth/me` returns the signed-in user, `PATCH /auth/me` renames them, `POST /auth/logout`
//! ends it.
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
use socialreins_core::{email, team, token};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{
    teams,
    waitlist::{self, error, rejected, Error, Reply},
};
use crate::{
    config::origin,
    models::{magic_links, sessions, users},
    rate_limit::{self, ClientIp},
    state::AppState,
    turnstile,
};

pub const SESSION_COOKIE: &str = "sr_session";
const LINK_MINUTES: i64 = 15;
const SESSION_DAYS: i64 = 30;
/// Unexpired links one email can hold; past it, `POST /auth/link` sends nothing.
const MAX_LIVE_LINKS: u64 = 3;
/// Longest display name, in characters after trimming.
const MAX_USER_NAME_CHARS: usize = 80;
/// "No account" emails one address can get a day from login attempts.
const LOGIN_NOTICES_PER_DAY: i64 = 3;

#[derive(Deserialize)]
struct LinkRequest {
    email: String,
    /// The name for a new account, from the sign-up form. An existing account keeps its own.
    name: Option<String>,
    /// From the login page: an email without an account gets a pointer to sign up, not a link.
    #[serde(default)]
    login_only: bool,
    turnstile_token: Option<String>,
}

pub(crate) fn internal(e: impl std::fmt::Display, what: &str) -> Response {
    tracing::error!(error = %e, what, "auth query failed");
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal").into_response()
}

/// Same 202, at the same speed, for a new email, a known one, and one refused by
/// `REGISTRATION_DISABLED` or `SIGNUP_DOMAINS` (which goes on the waitlist instead, see
/// `send_link`): the lookup, insert and mail all run after the reply, so neither the body nor
/// the timing says whether an account exists.
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
    let name = match body.name.as_deref() {
        Some(raw) => match team::name_up_to(raw, MAX_USER_NAME_CHARS) {
            Some(name) => Some(name),
            None => return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_name").into_response(),
        },
        None => None,
    };
    let token = body.turnstile_token.as_deref();
    if let Err(r) = turnstile::check(app.turnstile.as_ref(), token, ip).await {
        return r.into_response();
    }
    tokio::spawn(async move {
        if let Err(e) = send_link(&app, ip, email, name, body.login_only).await {
            tracing::error!(error = %e, "magic link not sent");
        }
    });
    (StatusCode::ACCEPTED, Json(json!({"status": "sent"}))).into_response()
}

/// A sign-in link to the signed-in user's own email, for confirming it is still them before an
/// action that needs a fresh sign-in (`reauth_required`). No body and no captcha: the session
/// names the email. The same per-IP limit and live-link cap as `POST /auth/link`.
async fn request_own_link(
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    _: FromWeb,
    CurrentUser(user, _): CurrentUser,
) -> Response {
    if !app.magic_link_limiter.allow(ip) {
        return error(StatusCode::TOO_MANY_REQUESTS, "rate_limited").into_response();
    }
    tokio::spawn(async move {
        if let Err(e) = send_link(&app, ip, user.email, None, true).await {
            tracing::error!(error = %e, "own magic link not sent");
        }
    });
    (StatusCode::ACCEPTED, Json(json!({"status": "sent"}))).into_response()
}

/// Errors carry the failure only, never the email or the token. A sign-up refused to a new email
/// puts it on the waitlist (source `signup`) under the waitlist's own rules: one confirmation
/// email at most hourly, within its per-IP and daily caps, none once verified. A refused login
/// attempt gets nothing.
async fn send_link(
    app: &AppState,
    ip: Option<std::net::IpAddr>,
    email: String,
    name: Option<String>,
    login_only: bool,
) -> Result<(), String> {
    let known = find_user(app, &email)
        .await
        .map_err(|e| format!("user lookup: {e}"))?
        .is_some();
    if !known
        && !may_sign_up(app, &email)
            .await
            .map_err(|e| format!("sign-up check: {e}"))?
    {
        if login_only {
            return Ok(());
        }
        let secret = waitlist::add(app, &email, Some("signup".into()))
            .await
            .map_err(|e| format!("waitlist add: {e}"))?;
        if let Some(secret) = secret {
            waitlist::send_confirmation(app, ip, &email, &secret, waitlist::SIGN_UP_CLOSED).await;
        }
        return Ok(());
    }
    if login_only && !known {
        return send_no_account_notice(app, &email).await;
    }
    let now = Utc::now().fixed_offset();
    magic_links::Entity::delete_many()
        .filter(magic_links::Column::ExpiresAt.lt(now))
        .exec(&app.db)
        .await
        .map_err(|e| format!("magic link cleanup: {e}"))?;
    let Some(secret) = insert_capped_link(app, &email, name)
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

/// Tells an email with no account how to create one, at most `LOGIN_NOTICES_PER_DAY` times. The
/// count is its own, so login attempts never use up the links a sign-up needs.
async fn send_no_account_notice(app: &AppState, email: &str) -> Result<(), String> {
    let caps = [(
        format!("login_notice:{}", token::hash(email)),
        LOGIN_NOTICES_PER_DAY,
    )];
    let reserved = rate_limit::daily_reserve(&app.db, &caps)
        .await
        .map_err(|e| format!("notice cap: {e}"))?;
    if reserved.is_none() {
        return Ok(());
    }
    let text = format!(
        "Someone tried to log in to SocialReins with this email, but it has no account yet.\n\n\
         To create one, sign up here:\n\n{}/signup\n\nIf this was not you, ignore this email.\n",
        app.web_url
    );
    app.mailer
        .send(email, "No SocialReins account for this email", text)
        .await
        .map_err(|e| format!("mail: {e}"))
}

/// Stores a new link unless `email` already holds `MAX_LIVE_LINKS` unexpired ones, which caps
/// mail to one inbox no matter how many IPs ask. The count and insert run under a per-email
/// advisory lock, so concurrent requests cannot both pass the check. Returns the new token.
async fn insert_capped_link(
    app: &AppState,
    email: &str,
    name: Option<String>,
) -> Result<Option<String>, DbErr> {
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
        name: Set(name),
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

/// The sign-up rules in force: `mode` is `open`, `closed` or `domains`.
pub(crate) struct SignupRules {
    pub mode: String,
    pub domains: Vec<String>,
    /// From `signup_settings` (the admin console), not from the env.
    pub saved: bool,
}

/// The rules saved from the admin console, else `REGISTRATION_DISABLED` and `SIGNUP_DOMAINS`.
/// The one place the sign-up gate and the admin console read them.
pub(crate) async fn signup_rules(app: &AppState) -> Result<SignupRules, DbErr> {
    let sql = "SELECT mode, domains FROM signup_settings";
    let stmt = Statement::from_string(DbBackend::Postgres, sql);
    if let Some(row) = app.db.query_one_raw(stmt).await? {
        let domains: serde_json::Value = row.try_get("", "domains")?;
        return Ok(SignupRules {
            mode: row.try_get("", "mode")?,
            // An unreadable list is an error, never an empty (open) one.
            domains: serde_json::from_value(domains)
                .map_err(|e| DbErr::Custom(format!("signup_settings.domains: {e}")))?,
            saved: true,
        });
    }
    let mode = if app.registration_disabled {
        "closed"
    } else if app.signup_domains.is_empty() {
        "open"
    } else {
        "domains"
    };
    Ok(SignupRules {
        mode: mode.into(),
        domains: app.signup_domains.clone(),
        saved: false,
    })
}

/// Mode `domains` admits anyone whose domain or exact address is listed, `open` anyone. `closed`
/// admits only, with no `OWNER_EMAIL`, whoever signs up first (from a listed domain, when any is
/// listed). `OWNER_EMAIL`, invited emails and approved waitlist emails are always admitted.
async fn sign_up(app: &AppState, email: &str) -> Result<SignUp, DbErr> {
    let rules = signup_rules(app).await?;
    // Domain entries have no `@` and email entries do, so one equality test serves both.
    // Only `closed` may have no list: `domains` with none admits nobody.
    let domain_ok = rules.mode == "open"
        || (rules.mode == "closed" && rules.domains.is_empty())
        || email
            .rsplit_once('@')
            .is_some_and(|(_, d)| rules.domains.iter().any(|a| a == d || a == email));
    if (domain_ok && rules.mode != "closed")
        || app.owner_email.as_deref() == Some(email)
        || teams::invited(&app.db, email).await?
        || waitlist::approved(app, email).await?
    {
        return Ok(SignUp::Allowed);
    }
    Ok(match app.owner_email {
        None if domain_ok => SignUp::FirstOnly,
        _ => SignUp::Refused,
    })
}

/// `sign_up` as a yes or no, without the lock `create_first_user` takes: for deciding whether
/// to send a link at all.
pub(crate) async fn may_sign_up(app: &AppState, email: &str) -> Result<bool, DbErr> {
    Ok(match sign_up(app, email).await? {
        SignUp::Allowed => true,
        SignUp::FirstOnly => users::Entity::find().one(&app.db).await?.is_none(),
        SignUp::Refused => false,
    })
}

/// `SignUp::FirstOnly`: creates the account only if none exists. The advisory lock makes
/// concurrent first sign-ups take turns, so exactly one sees an empty `users` table.
async fn create_first_user(
    app: &AppState,
    email: &str,
    name: Option<&str>,
) -> Result<Option<users::Model>, DbErr> {
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
    let user = create_user(&txn, email, name).await?;
    txn.commit().await?;
    Ok(Some(user))
}

/// The account for `email`, created under the sign-up rules when there is none (`true` when it
/// was); `None` when sign-up is closed to it. Every sign-in method creates accounts here.
pub(crate) async fn user_for_sign_in(
    app: &AppState,
    email: &str,
    name: Option<&str>,
) -> Result<Option<(users::Model, bool)>, DbErr> {
    if let Some(user) = find_user(app, email).await? {
        return Ok(Some((user, false)));
    }
    let created = match sign_up(app, email).await? {
        SignUp::Allowed => Some(create_user(&app.db, email, name).await?),
        SignUp::FirstOnly => create_first_user(app, email, name).await?,
        SignUp::Refused => None,
    };
    Ok(created.map(|user| (user, true)))
}

/// Lands on `{WEB_URL}/login?error=...` when the link cannot be used.
pub(crate) fn refused(app: &AppState, code: &str) -> Response {
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
         <meta name=\"viewport\" content=\"width=device-width\"><title>Sign in</title>\
         <link rel=\"icon\" href=\"/icon.svg\" type=\"image/svg+xml\"></head>\
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
    let user = match user_for_sign_in(&app, &link.email, link.name.as_deref()).await {
        Ok(Some((user, _))) => user,
        Ok(None) => return refused(&app, "registration_disabled"),
        Err(e) => return internal(e, "sign-in user"),
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

async fn create_user(
    db: &impl ConnectionTrait,
    email: &str,
    name: Option<&str>,
) -> Result<users::Model, DbErr> {
    let name = name.unwrap_or_else(|| email.split('@').next().unwrap_or_default());
    let row = users::ActiveModel {
        id: Set(uuid::Uuid::now_v7()),
        email: Set(email.to_owned()),
        name: Set(name.to_owned()),
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
    named_cookie(app, SESSION_COOKIE, "/", value, max_age)
}

/// An HttpOnly, SameSite=Lax cookie; `Secure` unless `BRAIN_URL` is plain http (local dev).
pub(crate) fn named_cookie(
    app: &AppState,
    name: &str,
    path: &str,
    value: &str,
    max_age: i64,
) -> String {
    let secure = if app.brain_url.starts_with("http://") {
        ""
    } else {
        "; Secure"
    };
    format!("{name}={value}; Path={path}; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}")
}

/// The value of cookie `name`.
pub(crate) fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(n, _)| *n == name)
        .map(|(_, value)| value)
}

fn session_token(headers: &HeaderMap) -> Option<&str> {
    cookie_value(headers, SESSION_COOKIE)
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
        match signed_in(app, &parts.headers).await {
            Ok(Some(current)) => Ok(current),
            Ok(None) => Err(error(StatusCode::UNAUTHORIZED, "unauthorized")),
            Err(e) => {
                tracing::error!(error = %e, "session lookup failed");
                Err(error(StatusCode::INTERNAL_SERVER_ERROR, "internal"))
            }
        }
    }
}

/// The user of an unexpired session cookie in `headers`, if any.
pub(crate) async fn signed_in(
    app: &AppState,
    headers: &HeaderMap,
) -> Result<Option<CurrentUser>, DbErr> {
    let Some(secret) = session_token(headers) else {
        return Ok(None);
    };
    let session = sessions::Entity::find_by_id(token::hash(secret))
        .filter(sessions::Column::ExpiresAt.gt(Utc::now().fixed_offset()))
        .one(&app.db)
        .await?;
    let Some(session) = session else {
        return Ok(None);
    };
    let user = users::Entity::find_by_id(session.user_id)
        .one(&app.db)
        .await?;
    Ok(user.map(|u| CurrentUser(u, session)))
}

/// The public fields of `user`, as `GET /auth/me` returns them.
pub(crate) fn user_json(user: &users::Model) -> Json<serde_json::Value> {
    Json(json!({
        "id": user.id,
        "email": user.email,
        "name": user.name,
        "username": user.username,
    }))
}

/// The user, plus `current_team` `{id, name, role}` and `is_admin` (in `ADMIN_EMAILS`).
async fn me(State(app): State<AppState>, CurrentUser(user, _): CurrentUser) -> Response {
    let (team, role) = match teams::current_team(&app.db, &user).await {
        Ok(found) => found,
        Err(e) => return internal(e, "current team lookup"),
    };
    let Json(mut body) = user_json(&user);
    body["current_team"] = json!({"id": team.id, "name": team.name, "role": role.as_str()});
    body["is_admin"] = super::waitlist::is_admin(&app, &user.email).into();
    Json(body).into_response()
}

#[derive(Deserialize, ToSchema)]
pub struct NewName {
    /// 1 to 80 characters after trimming, no control characters.
    pub name: String,
}

/// Changes the signed-in user's display name. Replies with the same body as `GET /auth/me`.
#[utoipa::path(patch, path = "/auth/me", tag = "auth", request_body = NewName,
    security(("session" = [])),
    responses((status = 200, body = serde_json::Value, description = "As `GET /auth/me`"),
        (status = 401, body = Error), (status = 403, body = Error), (status = 422, body = Error)))]
async fn rename(
    _: FromWeb,
    State(app): State<AppState>,
    CurrentUser(user, session): CurrentUser,
    Body(body): Body<NewName>,
) -> Response {
    let Some(name) = team::name_up_to(&body.name, MAX_USER_NAME_CHARS) else {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_name").into_response();
    };
    let row = users::ActiveModel {
        id: Set(user.id),
        name: Set(name),
        updated_at: Set(Utc::now().fixed_offset()),
        ..Default::default()
    };
    match users::Entity::update(row).exec(&app.db).await {
        Ok(user) => me(State(app), CurrentUser(user, session)).await,
        Err(e) => internal(e, "user rename"),
    }
}

/// `PATCH /auth/me`, documented in `/openapi.json` under its own `/` server (it is not in `/v1`).
pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(rename))
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
        .route("/auth/link/me", post(request_own_link))
        .route("/auth/magic", post(redeem))
        .route("/auth/magic/{token}", get(confirm_page))
        .route("/auth/me", get(me))
        .route("/auth/logout", post(logout))
}
