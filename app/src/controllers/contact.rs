//! `POST /v1/contact`: the socialreins.com contact form, mailed to `CONTACT_TO`.
use axum::{
    extract::{FromRequest, Request, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::waitlist::{error, rejected, Reply};
use crate::{rate_limit::ClientIp, state::AppState, turnstile};

const MAX_NAME_LEN: usize = 100;
const MAX_MESSAGE_LEN: usize = 5000;

#[derive(Deserialize, ToSchema)]
pub struct NewMessage {
    /// 1 to 100 characters, one line.
    pub name: String,
    /// Where the reply goes: the mail's `Reply-To`.
    pub email: String,
    /// 1 to 5000 characters.
    pub message: String,
    /// The Turnstile widget's token; required when `GET /auth/providers` gives a
    /// `turnstile_site_key`, else ignored.
    pub turnstile_token: Option<String>,
}

/// `202 {}` once the message is mailed. Each client IP gets 5 an hour.
#[utoipa::path(post, path = "/contact", tag = "contact", request_body = NewMessage,
    responses((status = 202, body = serde_json::Value), (status = 400, body = super::waitlist::Error),
        (status = 413, body = super::waitlist::Error), (status = 415, body = super::waitlist::Error),
        (status = 422, body = super::waitlist::Error), (status = 429, body = super::waitlist::Error),
        (status = 500, body = super::waitlist::Error), (status = 503, body = super::waitlist::Error)))]
async fn send(State(app): State<AppState>, ClientIp(ip): ClientIp, request: Request) -> Reply {
    let Some(to) = app.contact_to.clone() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "contact_unavailable");
    };
    if !app.contact_limiter.allow(ip) {
        return error(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let body = match Json::<NewMessage>::from_request(request, &()).await {
        Ok(Json(body)) => body,
        Err(r) => return rejected(&r),
    };
    let invalid = |code| error(StatusCode::UNPROCESSABLE_ENTITY, code);
    let name = body.name.trim();
    // No CR or LF: the name goes into the subject header.
    if name.is_empty() || name.chars().count() > MAX_NAME_LEN || name.contains(['\r', '\n']) {
        return invalid("invalid_name");
    }
    let email = (!body.email.contains(['\r', '\n']))
        .then(|| socialreins_core::email::normalize(&body.email))
        .flatten();
    let Some(email) = email else {
        return invalid("invalid_email");
    };
    let message = body.message.trim();
    if message.is_empty() || message.chars().count() > MAX_MESSAGE_LEN {
        return invalid("invalid_message");
    }
    let token = body.turnstile_token.as_deref();
    if let Err(r) = turnstile::check(app.turnstile.as_ref(), token, ip).await {
        return r;
    }
    let text = format!("Name: {name}\nEmail: {email}\n\n{message}\n");
    let subject = format!("Contact form: {name}");
    let sent = app.mailer.send_reply_to(&to, Some(&email), &subject, text);
    if let Err(e) = sent.await {
        tracing::error!(error = %e, "contact message not sent");
        return error(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    }
    (StatusCode::ACCEPTED, Json(serde_json::json!({})))
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(send))
}
