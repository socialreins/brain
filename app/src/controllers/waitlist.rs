//! `POST /v1/waitlist`: the socialreins.com signup form.
use axum::{
    extract::{rejection::JsonRejection, FromRequest, Request, State},
    http::StatusCode,
    Json,
};
use sea_orm::{ActiveValue::Set, EntityTrait};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{models::waitlist_signups, rate_limit::ClientIp, state::AppState};

const MAX_SOURCE_LEN: usize = 100;

#[derive(Deserialize, ToSchema)]
pub struct NewSignup {
    pub email: String,
    /// Where the signup came from, e.g. `landing-hero`. At most 100 characters.
    pub source: Option<String>,
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
    /// e.g. `invalid_email`, `rate_limited`, `unauthorized`, `forbidden_origin`, `not_found`.
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
        id: Set(uuid::Uuid::now_v7()),
        email: Set(email),
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
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(join))
}
