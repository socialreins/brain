//! The socialreins.com waitlist: `POST /v1/waitlist` joins and emails a confirmation link to the
//! web app, which posts its token to `POST /v1/waitlist/verify`, `GET /v1/waitlist/count` counts every signup, and
//! `GET /v1/admin/waitlist` lists them for `ADMIN_EMAILS` (approve and delete: `admin.rs`).
use std::time::{Duration as StdDuration, Instant};

use axum::{
    extract::{
        rejection::{JsonRejection, QueryRejection},
        FromRequest, Query, Request, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Duration, FixedOffset, Utc};
use sea_orm::{
    sea_query::Expr, ActiveValue::Set, ColumnTrait, Condition, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect,
};
use serde::{Deserialize, Serialize};
use socialreins_core::token;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::auth::{internal, CurrentUser};
use crate::{
    models::waitlist_signups,
    rate_limit::{self, daily_refund, daily_reserve, ClientIp},
    state::AppState,
};

const MAX_SOURCE_LEN: usize = 100;
/// An unverified email gets at most one confirmation email per hour, however often it signs up.
const RESEND_AFTER_HOURS: i64 = 1;
/// Confirmation emails across all signups per rolling 24 hours, since the form has no captcha.
const CONFIRMATIONS_PER_DAY: i64 = 500;
/// Confirmation emails one client IP (IPv6 by /64) may trigger per rolling 24 hours, so one
/// source cannot use up the global allowance.
const CONFIRMATIONS_PER_IP_PER_DAY: i64 = 20;
const CONFIRMATIONS_BUCKET: &str = "waitlist:confirm";
/// How long `GET /v1/waitlist/count` serves its cached number.
const COUNT_TTL: StdDuration = StdDuration::from_secs(30);
/// shortcut: the admin list stops at the newest 1000 signups, add paging once the list is longer.
const ADMIN_LIST_LIMIT: u64 = 1000;

#[derive(Deserialize, ToSchema)]
pub struct NewSignup {
    pub email: String,
    /// Where the signup came from, e.g. `landing-hero`. At most 100 characters.
    pub source: Option<String>,
    /// Ignored: accepted so older clients that still send a Turnstile token keep working.
    pub turnstile_token: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct Status {
    /// Always `ok`: the response never says whether the email was already on the list.
    pub status: String,
}

#[derive(Serialize, ToSchema)]
pub struct Error {
    /// A machine-readable code. Every route may answer `invalid_json`, `invalid_body`,
    /// `unsupported_media_type`, `payload_too_large` or `internal`; the others are per route,
    /// e.g. `invalid_email`, `invalid_source`, `invalid_query`, `rate_limited`, `unauthorized`, `forbidden`,
    /// `forbidden_origin`, `not_found`, and `captcha_failed` or `captcha_unavailable` on `/contact`.
    pub error: String,
}

pub(crate) type Reply = (StatusCode, Json<serde_json::Value>);

pub(crate) fn error(status: StatusCode, code: &str) -> Reply {
    (status, Json(serde_json::json!({"error": code})))
}

/// A body axum could not read as `NewSignup`, as a JSON error instead of axum's plain text.
pub(crate) fn rejected(r: &JsonRejection) -> Reply {
    let code = match r {
        JsonRejection::JsonSyntaxError(_) => "invalid_json",
        JsonRejection::MissingJsonContentType(_) => "unsupported_media_type",
        _ if r.status() == StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
        _ => "invalid_body",
    };
    error(r.status(), code)
}

/// `200 {"status":"ok"}` for a new email and a known one alike. A new or unverified email gets a
/// confirmation link by email, at most once an hour, 20 a day per IP and 500 a day overall; the mail goes
/// out after the reply, so neither the body nor the timing says whether the email was already on
/// the list.
#[utoipa::path(post, path = "/waitlist", tag = "waitlist", request_body = NewSignup,
    responses((status = 200, body = Status), (status = 400, body = Error),
        (status = 413, body = Error), (status = 415, body = Error), (status = 422, body = Error),
        (status = 429, body = Error), (status = 500, body = Error)))]
async fn join(State(app): State<AppState>, ClientIp(ip): ClientIp, request: Request) -> Reply {
    // Counted before the body is read, so malformed requests use up the limit too.
    if !app.waitlist_limiter.allow(ip) {
        return error(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let body = match Json::<NewSignup>::from_request(request, &()).await {
        Ok(Json(body)) => body,
        Err(r) => return rejected(&r),
    };
    let Some(email) = socialreins_core::email::normalize(&body.email) else {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_email");
    };
    let source = body
        .source
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    if source
        .as_ref()
        .is_some_and(|s| s.chars().count() > MAX_SOURCE_LEN)
    {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_source");
    }
    let secret = match add(&app, &email, source).await {
        Ok(secret) => secret,
        Err(e) => {
            tracing::error!(error = %e, "waitlist join failed");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "internal");
        }
    };
    if let Some(secret) = secret {
        // A failed mail keeps the row and the ok reply; the error carries no email or token.
        tokio::spawn(async move { send_confirmation(&app, ip, &email, &secret, JOINED).await });
    }
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
}

/// The confirmation email's first line after `POST /v1/waitlist`.
pub(crate) const JOINED: &str = "Thanks for joining the SocialReins waitlist.";
/// The confirmation email's first line when sign-up refused the email and put it on the list.
pub(crate) const SIGN_UP_CLOSED: &str = "Sign-ups are closed for now. We've added you to the \
    waitlist and will email you when SocialReins is live.";

/// Puts `email` on the list (a known email keeps its first row and source) and claims a
/// confirmation for it (`claim_confirmation`). Returns the token to mail, if any.
pub(crate) async fn add(
    app: &AppState,
    email: &str,
    source: Option<String>,
) -> Result<Option<String>, sea_orm::DbErr> {
    let row = waitlist_signups::ActiveModel {
        id: Set(Uuid::now_v7()),
        email: Set(email.to_owned()),
        source: Set(source),
        ..Default::default()
    };
    // DO NOTHING on a known email: the first signup (and its source) wins, and the reply is the same.
    waitlist_signups::Entity::insert(row)
        .on_conflict_do_nothing_on([waitlist_signups::Column::Email])
        .exec_without_returning(&app.db)
        .await?;
    claim_confirmation(app, email).await
}

/// Puts `email` on the list as verified, for a mailbox Google vouches for. A known email keeps
/// its row and source and becomes verified if it was not.
pub(crate) async fn add_verified(
    app: &AppState,
    email: &str,
    source: &str,
) -> Result<(), sea_orm::DbErr> {
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO waitlist_signups (id, email, source, verified_at) VALUES ($1, $2, $3, now()) \
         ON CONFLICT (email) DO UPDATE \
         SET verified_at = COALESCE(waitlist_signups.verified_at, now())",
        [Uuid::now_v7().into(), email.into(), source.into()],
    );
    app.db.execute_raw(stmt).await.map(|_| ())
}

/// Gives the unverified `email` a new link token unless one went out in the last hour, and
/// returns it. One UPDATE checks and stamps, so concurrent signups cannot both send. A resend
/// replaces the token, so only the newest email's link works.
async fn claim_confirmation(app: &AppState, email: &str) -> Result<Option<String>, sea_orm::DbErr> {
    use waitlist_signups::Column;
    let now = Utc::now().fixed_offset();
    let secret = token::new();
    let claimed = waitlist_signups::Entity::update_many()
        .col_expr(Column::VerifyTokenHash, Expr::value(token::hash(&secret)))
        .col_expr(Column::VerifySentAt, Expr::value(now))
        .filter(Column::Email.eq(email))
        .filter(Column::VerifiedAt.is_null())
        .filter(
            Condition::any()
                .add(Column::VerifySentAt.is_null())
                .add(Column::VerifySentAt.lt(now - Duration::hours(RESEND_AFTER_HOURS))),
        )
        .exec(&app.db)
        .await?;
    Ok((claimed.rows_affected == 1).then_some(secret))
}

/// Mails the link within the daily caps, per IP and global; both are checked before either is
/// counted, so a refused IP uses up none of the global allowance. Unsent (a cap reached or mail
/// failed), the claim is undone so the next signup of this email may try again; logs carry no
/// email or token.
pub(crate) async fn send_confirmation(
    app: &AppState,
    ip: Option<std::net::IpAddr>,
    email: &str,
    secret: &str,
    intro: &str,
) {
    let ip = ip.map_or_else(|| "unknown".into(), |ip| rate_limit::bucket(ip).to_string());
    let caps = [
        (
            format!("{CONFIRMATIONS_BUCKET}:ip:{ip}"),
            CONFIRMATIONS_PER_IP_PER_DAY,
        ),
        (CONFIRMATIONS_BUCKET.to_owned(), CONFIRMATIONS_PER_DAY),
    ];
    let reserved = match daily_reserve(&app.db, &caps).await {
        Ok(Some(ids)) => ids,
        Ok(None) => {
            tracing::warn!("waitlist confirmation not sent: daily cap reached");
            return unclaim(app, secret).await;
        }
        Err(e) => {
            tracing::error!(error = %e, "waitlist confirmation cap check failed");
            return unclaim(app, secret).await;
        }
    };
    let url = format!("{}/waitlist/verify?token={secret}", app.web_url);
    let text = format!(
        "{intro}\n\nConfirm my email:\n{url}\n\nIf you did not sign up, ignore this email.\n"
    );
    let subject = "Confirm your spot on the SocialReins waitlist";
    if let Err(e) = app.mailer.send(email, subject, text).await {
        tracing::error!(error = %e, "waitlist confirmation not sent");
        daily_refund(&app.db, &reserved).await;
        unclaim(app, secret).await;
    }
}

/// Clears `verify_sent_at` of the claim holding `secret`, so it does not hold back a resend.
async fn unclaim(app: &AppState, secret: &str) {
    use waitlist_signups::Column;
    let cleared = waitlist_signups::Entity::update_many()
        .col_expr(
            Column::VerifySentAt,
            Expr::value(None::<DateTime<FixedOffset>>),
        )
        .filter(Column::VerifyTokenHash.eq(token::hash(secret)))
        .exec(&app.db)
        .await;
    if let Err(e) = cleared {
        tracing::warn!(error = %e, "waitlist confirmation unclaim failed");
    }
}

#[derive(Deserialize, ToSchema)]
pub struct VerifyBody {
    /// The `token` query parameter of the emailed link.
    pub token: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct Verified {
    /// Always `verified`, also when the email already was.
    pub status: String,
}

/// Confirms an email with the token from its confirmation link, which opens
/// `{WEB_URL}/waitlist/verify?token=...`; the web page posts it here, so a mail scanner that
/// opens the link changes nothing. The first call sets `verified_at`, later ones keep it.
/// `422 invalid_link` for a missing, malformed or unknown token. Shares the waitlist IP limit.
#[utoipa::path(post, path = "/waitlist/verify", tag = "waitlist", request_body = VerifyBody,
    responses((status = 200, body = Verified), (status = 400, body = Error),
        (status = 413, body = Error), (status = 415, body = Error), (status = 422, body = Error),
        (status = 429, body = Error), (status = 500, body = Error)))]
async fn verify(State(app): State<AppState>, ClientIp(ip): ClientIp, request: Request) -> Reply {
    use waitlist_signups::Column;
    if !app.waitlist_limiter.allow(ip) {
        return error(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let secret = match Json::<VerifyBody>::from_request(request, &()).await {
        Ok(Json(body)) => body.token.unwrap_or_default(),
        Err(r) => return rejected(&r),
    };
    let invalid = || error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_link");
    if secret.len() != 64 || !secret.bytes().all(|b| b.is_ascii_hexdigit()) {
        return invalid();
    }
    let marked = waitlist_signups::Entity::update_many()
        .col_expr(
            Column::VerifiedAt,
            Expr::cust("COALESCE(verified_at, now())"),
        )
        .filter(Column::VerifyTokenHash.eq(token::hash(&secret)))
        .exec(&app.db)
        .await;
    match marked {
        Ok(r) if r.rows_affected == 1 => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "verified"})),
        ),
        Ok(_) => invalid(),
        Err(e) => {
            tracing::error!(error = %e, "waitlist verify failed");
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal")
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct Count {
    /// Every signup, verified or not. Up to 30 seconds old.
    pub count: u64,
}

/// The number of signups, for the landing page. Cached in memory for 30 seconds.
#[utoipa::path(get, path = "/waitlist/count", tag = "waitlist",
    responses((status = 200, body = Count), (status = 500, body = Error)))]
async fn count(State(app): State<AppState>) -> Response {
    let cached = *app
        .waitlist_count
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, n)) = cached.filter(|(at, _)| at.elapsed() < COUNT_TTL) {
        return Json(Count { count: n }).into_response();
    }
    let n = match waitlist_signups::Entity::find().count(&app.db).await {
        Ok(n) => n,
        Err(e) => return internal(e, "waitlist count"),
    };
    *app.waitlist_count
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((Instant::now(), n));
    Json(Count { count: n }).into_response()
}

#[derive(Deserialize, IntoParams)]
pub struct AdminQuery {
    /// Only verified (`true`) or unverified (`false`) signups; both when absent.
    pub verified: Option<bool>,
    /// Only signups whose email contains this, in any case.
    pub q: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct AdminSignup {
    pub id: Uuid,
    pub email: String,
    pub source: Option<String>,
    pub created_at: DateTime<FixedOffset>,
    pub verified_at: Option<DateTime<FixedOffset>>,
    /// When an admin approved the email for sign-up.
    pub approved_at: Option<DateTime<FixedOffset>>,
}

impl From<waitlist_signups::Model> for AdminSignup {
    fn from(r: waitlist_signups::Model) -> Self {
        Self {
            id: r.id,
            email: r.email,
            source: r.source,
            created_at: r.created_at,
            verified_at: r.verified_at,
            approved_at: r.approved_at,
        }
    }
}

/// `%q%` for `LIKE`, with the wildcards in `q` escaped.
pub(crate) fn like(q: &str) -> String {
    let escaped = q
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// An admin approved `email` on the waitlist, so it may sign up whatever the sign-up rules say.
pub(crate) async fn approved(app: &AppState, email: &str) -> Result<bool, sea_orm::DbErr> {
    use waitlist_signups::Column;
    let found = waitlist_signups::Entity::find()
        .filter(Column::Email.eq(email))
        .filter(Column::ApprovedAt.is_not_null())
        .count(&app.db)
        .await?;
    Ok(found > 0)
}

#[derive(Serialize, ToSchema)]
pub struct AdminWaitlist {
    /// Every signup, whatever the filter.
    pub total: u64,
    /// Verified signups, whatever the filter.
    pub verified: u64,
    /// Newest first, at most 1000.
    pub signups: Vec<AdminSignup>,
}

/// Whether `email` is in `ADMIN_EMAILS`.
pub(crate) fn is_admin(app: &AppState, email: &str) -> bool {
    app.admin_emails.iter().any(|a| a == email)
}

/// Every signup with whether it is verified, for a user in `ADMIN_EMAILS` signed in with the
/// session cookie (API tokens do not reach it).
#[utoipa::path(get, path = "/admin/waitlist", tag = "admin",
    security(("session" = [])), params(AdminQuery),
    responses((status = 200, body = AdminWaitlist), (status = 401, body = Error),
        (status = 403, body = Error), (status = 422, body = Error), (status = 500, body = Error)))]
async fn admin_list(
    State(app): State<AppState>,
    CurrentUser(user, _): CurrentUser,
    q: Result<Query<AdminQuery>, QueryRejection>,
) -> Response {
    use waitlist_signups::{Column, Entity};
    if !is_admin(&app, &user.email) {
        return error(StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    let Ok(Query(q)) = q else {
        return error(StatusCode::UNPROCESSABLE_ENTITY, "invalid_query").into_response();
    };
    let found = async {
        let total = Entity::find().count(&app.db).await?;
        let verified = Entity::find()
            .filter(Column::VerifiedAt.is_not_null())
            .count(&app.db)
            .await?;
        let mut list = Entity::find();
        if let Some(v) = q.verified {
            list = list.filter(if v {
                Column::VerifiedAt.is_not_null()
            } else {
                Column::VerifiedAt.is_null()
            });
        }
        if let Some(q) = q.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
            // Stored emails are lowercase, so lowercasing `q` makes the match case-insensitive.
            // Cut at 200 characters, the cap the other admin lists put on `q`.
            let q: String = q.to_lowercase().chars().take(200).collect();
            list = list.filter(Column::Email.like(like(&q)));
        }
        let rows = list
            .order_by_desc(Column::CreatedAt)
            .order_by_desc(Column::Id)
            .limit(ADMIN_LIST_LIMIT)
            .all(&app.db)
            .await?;
        Ok::<_, sea_orm::DbErr>((total, verified, rows))
    };
    let (total, verified, rows) = match found.await {
        Ok(found) => found,
        Err(e) => return internal(e, "admin waitlist"),
    };
    let signups = rows.into_iter().map(AdminSignup::from).collect();
    Json(AdminWaitlist {
        total,
        verified,
        signups,
    })
    .into_response()
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(join))
        .routes(routes!(verify))
        .routes(routes!(count))
        .routes(routes!(admin_list))
}
