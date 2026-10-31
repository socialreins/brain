//! The current team's outbound webhook (see `crate::webhooks`): owners and admins read, set,
//! delete and test it with a session. The signing secret is answered once, when it is made.
use axum::{extract::State, http::StatusCode, response::IntoResponse, response::Response, Json};
use chrono::{DateTime, FixedOffset, Utc};
use sea_orm::{ConnectionTrait, DbBackend, QueryResult, Statement};
use serde::{Deserialize, Serialize};
use serde_json::json;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    auth::{internal, Body, FromWeb},
    teams::InTeam,
    waitlist::{error, Error},
};
use crate::{state::AppState, webhooks::new_secret};

#[derive(Serialize, ToSchema)]
pub struct Webhook {
    pub url: String,
    pub created_at: DateTime<FixedOffset>,
    pub updated_at: DateTime<FixedOffset>,
}

#[derive(Serialize, ToSchema)]
pub struct NewWebhookReply {
    pub webhook: Webhook,
    /// The signing secret. Shown once, never again.
    pub secret: String,
}

#[derive(Deserialize, ToSchema)]
pub struct WebhookInput {
    /// A public `https://` URL, at most 2000 characters, without credentials.
    pub url: String,
}

#[derive(Serialize, ToSchema)]
pub struct PingReply {
    /// The receiver's HTTP status; `null` when it was not reached.
    pub status: Option<u16>,
    /// `blocked_address`, `timeout` or `unreachable` when it was not reached.
    pub error: Option<String>,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn stmt(sql: &str, values: Vec<sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

fn webhook_json(r: &QueryResult) -> Result<Webhook, sea_orm::DbErr> {
    Ok(Webhook {
        url: r.try_get("", "url")?,
        created_at: r.try_get("", "created_at")?,
        updated_at: r.try_get("", "updated_at")?,
    })
}

/// The current team's webhook, without its secret; `404` when it has none. Owners and admins.
#[utoipa::path(get, path = "/team/webhook", tag = "webhooks",
    responses((status = 200, body = Webhook), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error)))]
async fn show(State(app): State<AppState>, t: InTeam) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    let found = app
        .db
        .query_one_raw(stmt(
            "SELECT url, created_at, updated_at FROM team_webhooks WHERE team_id = $1",
            vec![t.team.id.into()],
        ))
        .await;
    match found.and_then(|r| r.as_ref().map(webhook_json).transpose()) {
        Ok(Some(w)) => Json(w).into_response(),
        Ok(None) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, "webhook read"),
    }
}

/// Sets the webhook URL. A new webhook gets a signing secret, answered once (`201`); a change
/// keeps the secret (`200`). Owners and admins, with a sign-in from the last 10 minutes (else
/// `403 reauth_required`).
#[utoipa::path(put, path = "/team/webhook", tag = "webhooks", request_body = WebhookInput,
    responses((status = 201, body = NewWebhookReply), (status = 200, body = Webhook),
        (status = 401, body = Error), (status = 403, body = Error), (status = 422, body = Error)))]
async fn set(
    _: FromWeb,
    State(app): State<AppState>,
    t: InTeam,
    Body(body): Body<WebhookInput>,
) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    if !t.current.recent() {
        return reply(StatusCode::FORBIDDEN, "reauth_required");
    }
    let Some(url) = app.webhooks.check_url(body.url.trim()) else {
        return reply(StatusCode::UNPROCESSABLE_ENTITY, "invalid_url");
    };
    let secret = new_secret();
    let saved = app
        .db
        .query_one_raw(stmt(
            "INSERT INTO team_webhooks (team_id, url, secret) VALUES ($1, $2, $3) \
             ON CONFLICT (team_id) DO UPDATE SET url = EXCLUDED.url, updated_at = now() \
             RETURNING url, created_at, updated_at, (xmax = 0) AS created",
            vec![
                t.team.id.into(),
                url.as_str().into(),
                app.cipher.encrypt(secret.as_bytes()).into(),
            ],
        ))
        .await;
    let row = match saved {
        Ok(Some(r)) => r,
        Ok(None) => return internal("no row", "webhook upsert"),
        Err(e) => return internal(e, "webhook upsert"),
    };
    let (webhook, created) = match (webhook_json(&row), row.try_get::<bool>("", "created")) {
        (Ok(w), Ok(c)) => (w, c),
        (Err(e), _) | (_, Err(e)) => return internal(e, "webhook upsert"),
    };
    if created {
        let body = NewWebhookReply { webhook, secret };
        (StatusCode::CREATED, Json(body)).into_response()
    } else {
        Json(webhook).into_response()
    }
}

/// Deletes the webhook and its pending deliveries, `204`. Owners and admins.
#[utoipa::path(delete, path = "/team/webhook", tag = "webhooks",
    responses((status = 204), (status = 401, body = Error), (status = 403, body = Error),
        (status = 404, body = Error)))]
async fn destroy(_: FromWeb, State(app): State<AppState>, t: InTeam) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    let done = app
        .db
        .execute_raw(stmt(
            "DELETE FROM team_webhooks WHERE team_id = $1",
            vec![t.team.id.into()],
        ))
        .await;
    match done {
        Ok(r) if r.rows_affected() == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => internal(e, "webhook delete"),
    }
}

/// Sends a signed `ping` event now and answers what the receiver said. Owners and admins; 10 a
/// minute per user, shared with publish now.
#[utoipa::path(post, path = "/team/webhook/test", tag = "webhooks",
    responses((status = 200, body = PingReply), (status = 401, body = Error),
        (status = 403, body = Error), (status = 404, body = Error), (status = 429, body = Error)))]
async fn test(_: FromWeb, State(app): State<AppState>, t: InTeam) -> Response {
    if !t.role.manages() {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    if !app.post_limiter.hit(t.current.0.id) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let found = app
        .db
        .query_one_raw(stmt(
            "SELECT url, secret FROM team_webhooks WHERE team_id = $1",
            vec![t.team.id.into()],
        ))
        .await;
    let row = match found {
        Ok(Some(r)) => r,
        Ok(None) => return reply(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => return internal(e, "webhook read"),
    };
    let (url, sealed) = match (
        row.try_get::<String>("", "url"),
        row.try_get::<Vec<u8>>("", "secret"),
    ) {
        (Ok(u), Ok(s)) => (u, s),
        (Err(e), _) | (_, Err(e)) => return internal(e, "webhook read"),
    };
    let Some(secret) = app
        .cipher
        .decrypt(&sealed)
        .and_then(|b| String::from_utf8(b).ok())
    else {
        return internal("secret not readable", "webhook secret");
    };
    let id = Uuid::now_v7();
    let body = json!({"id": id, "type": "ping", "created_at": Utc::now(), "data": {}});
    let sent = app
        .webhooks
        .send(&url, &secret, "ping", id, body.to_string())
        .await;
    let (status, error) = match sent {
        Ok(code) => (Some(code), None),
        Err(code) => (None, Some(code.to_owned())),
    };
    Json(PingReply { status, error }).into_response()
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(show, set, destroy))
        .routes(routes!(test))
}
