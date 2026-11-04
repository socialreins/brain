//! The Meta webhook, `/webhooks/meta`, registered in the Meta app (callback URL
//! `{BRAIN_URL}/webhooks/meta`, verify token `META_WEBHOOK_VERIFY_TOKEN`). `GET` answers the
//! subscribe handshake. `POST` takes the deliveries for Pages (`object=page`: feed comments and
//! Messenger messages) and Instagram (`comments`, `messages`): the signature of the raw body is
//! checked before anything is read, then the items are stored (`meta_inbox`) and the rest of
//! the work (names, links, automations) runs after the `200`.
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
use crate::{meta_inbox, state::AppState};

/// Largest delivery taken. Meta batches events; a few hundred fit in this.
pub(crate) const MAX_BODY_BYTES: usize = 256 * 1024;

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

/// `GET /webhooks/meta`: the handshake. `hub.mode=subscribe` and a `hub.verify_token` equal to
/// `META_WEBHOOK_VERIFY_TOKEN` get `hub.challenge` back as plain text; `403` otherwise, `404`
/// while the variable is unset.
async fn verify(State(app): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(token) = app.meta.webhook_token.as_deref() else {
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

/// `POST /webhooks/meta`: `403` unless `X-Hub-Signature-256` is the HMAC-SHA256 of the raw body
/// under `META_APP_SECRET` (`404` while Meta is not set up). A signed delivery is answered
/// `200` once its items are stored, whatever it holds; a database failure is a `500`, so Meta
/// sends it again (storing is idempotent).
async fn receive(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if !app.meta.configured() {
        return reply(StatusCode::NOT_FOUND, "not_found");
    }
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !app.meta.signed(&body, signature) {
        return reply(StatusCode::FORBIDDEN, "forbidden");
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        tracing::warn!("meta webhook: signed body is not JSON");
        return StatusCode::OK.into_response();
    };
    let mut fresh = Vec::new();
    let stored = meta_inbox::ingest(&app, &payload, &mut fresh).await;
    // Even after an error: a redelivery finds these items stored and would not run them again.
    if !fresh.is_empty() {
        let later = app.clone();
        tokio::spawn(async move { meta_inbox::follow_up(&later, fresh).await });
    }
    match stored {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => internal(e, "meta webhook"),
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/webhooks/meta", get(verify).post(receive))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
}
