//! The socialreins.com waitlist: `POST /v1/waitlist` joins and emails a confirmation link,
//! `GET /v1/waitlist/verify` is that link, `GET /v1/waitlist/count` counts every signup, and
//! `GET /v1/admin/waitlist` lists them for `ADMIN_EMAILS`.
use std::time::{Duration as StdDuration, Instant};

use axum::{
    extract::{
        rejection::{JsonRejection, QueryRejection},
        FromRequest, Query, Request, State,
    },
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
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
use crate::{models::waitlist_signups, rate_limit::ClientIp, state::AppState};

const MAX_SOURCE_LEN: usize = 100;
/// An unverified email gets at most one confirmation email per hour, however often it signs up.
const RESEND_AFTER_HOURS: i64 = 1;
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
/// confirmation link by email, at most once an hour; the mail goes out after the reply, so
/// neither the body nor the timing says whether the email was already on the list.
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
    let row = waitlist_signups::ActiveModel {
        id: Set(Uuid::now_v7()),
        email: Set(email.clone()),
        source: Set(source),
        ..Default::default()
    };
    // DO NOTHING on a known email: the first signup (and its source) wins, and the reply is the same.
    let insert = waitlist_signups::Entity::insert(row)
        .on_conflict_do_nothing_on([waitlist_signups::Column::Email])
        .exec_without_returning(&app.db)
        .await;
    if let Err(e) = insert {
        tracing::error!(error = %e, "waitlist insert failed");
        return error(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    }
    let secret = match claim_confirmation(&app, &email).await {
        Ok(secret) => secret,
        Err(e) => {
            tracing::error!(error = %e, "waitlist confirmation claim failed");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "internal");
        }
    };
    if let Some(secret) = secret {
        // A failed mail keeps the row and the ok reply; the error carries no email or token.
        tokio::spawn(async move {
            if let Err(e) = send_confirmation(&app, &email, &secret).await {
                tracing::error!(error = %e, "waitlist confirmation not sent");
            }
        });
    }
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
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

async fn send_confirmation(app: &AppState, email: &str, secret: &str) -> Result<(), String> {
    let url = format!("{}/v1/waitlist/verify?token={secret}", app.brain_url);
    let text = format!(
        "Thanks for joining the SocialReins waitlist.\n\nConfirm my email:\n{url}\n\nIf you \
         did not sign up, ignore this email.\n"
    );
    app.mailer
        .send(email, "Confirm your spot on the SocialReins waitlist", text)
        .await
        .map_err(|e| format!("mail: {e}"))
}

#[derive(Deserialize, IntoParams)]
pub struct VerifyQuery {
    /// The token from the confirmation email.
    pub token: String,
}

/// The link in the confirmation email: marks the signup verified (the first click sets
/// `verified_at`, later ones keep it) and redirects (303) to `{WEB_URL}/waitlist/verified`, or
/// to `{WEB_URL}/waitlist/verified?error=invalid_link` for an unknown or malformed token.
#[utoipa::path(get, path = "/waitlist/verify", tag = "waitlist", params(VerifyQuery),
    responses((status = 303, description = "Redirect to the web app"),
        (status = 500, body = Error)))]
async fn verify(
    State(app): State<AppState>,
    q: Result<Query<VerifyQuery>, QueryRejection>,
) -> Response {
    use waitlist_signups::Column;
    let done = format!("{}/waitlist/verified", app.web_url);
    let invalid = || Redirect::to(&format!("{done}?error=invalid_link")).into_response();
    let Ok(Query(VerifyQuery { token: secret })) = q else {
        return invalid();
    };
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
        Ok(r) if r.rows_affected == 1 => Redirect::to(&done).into_response(),
        Ok(_) => invalid(),
        Err(e) => internal(e, "waitlist verify"),
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
}

#[derive(Serialize, ToSchema)]
pub struct AdminSignup {
    pub id: Uuid,
    pub email: String,
    pub source: Option<String>,
    pub created_at: DateTime<FixedOffset>,
    pub verified_at: Option<DateTime<FixedOffset>>,
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
    let signups = rows
        .into_iter()
        .map(|r| AdminSignup {
            id: r.id,
            email: r.email,
            source: r.source,
            created_at: r.created_at,
            verified_at: r.verified_at,
        })
        .collect();
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
