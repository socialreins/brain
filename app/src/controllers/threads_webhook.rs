//! The Threads webhook, `/webhooks/threads`, registered in the Threads app (callback URL
//! `{BRAIN_URL}/webhooks/threads`, verify token `THREADS_WEBHOOK_VERIFY_TOKEN`). Threads delivers
//! `replies` and `mentions` only to an approved app, so brain polls anyway; a delivery just asks
//! for an early read of the accounts it names (`threads_inbox::hint`) and stores nothing itself.
//! That keeps a delivery's worth small, and every delivery must carry a right signature.
use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use serde_json::Value;

use super::{auth::internal, waitlist::error};
use crate::{state::AppState, threads_inbox};

/// Largest delivery taken.
const MAX_BODY_BYTES: usize = 256 * 1024;

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

/// `GET /webhooks/threads`: the handshake, as Meta's: `hub.mode=subscribe` and a
/// `hub.verify_token` equal to `THREADS_WEBHOOK_VERIFY_TOKEN` get `hub.challenge` back as plain
/// text; `403` otherwise, `404` while the variable is unset.
async fn verify(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(token) = app.threads.webhook_token.as_deref() else {
        return reply(StatusCode::NOT_FOUND, "not_found");
    };
    let sent = q.get("hub.verify_token").map_or("", String::as_str);
    let ok = q.get("hub.mode").map(String::as_str) == Some("subscribe")
        && socialreins_core::token::same(token, sent);
    match (ok, q.get("hub.challenge")) {
        (true, Some(challenge)) => (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            challenge.clone(),
        )
            .into_response(),
        _ => reply(StatusCode::FORBIDDEN, "forbidden"),
    }
}

/// `POST /webhooks/threads`: `404` while the webhook is off, `403` for an `X-Hub-Signature-256`
/// that is not the HMAC-SHA256 of the raw body under `THREADS_APP_SECRET` (none is not one), else
/// `200`.
async fn receive(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if app.threads.webhook_token.is_none() {
        return reply(StatusCode::NOT_FOUND, "not_found");
    }
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !app.threads.signed(&body, signature) {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        return StatusCode::OK.into_response();
    };
    match threads_inbox::hint(&app, &threads_inbox::users_in(&payload)).await {
        Ok(_) => StatusCode::OK.into_response(),
        Err(e) => internal(e, "threads webhook"),
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/webhooks/threads", get(verify).post(receive))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
}
