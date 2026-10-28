use std::sync::Arc;

use axum::http::HeaderValue;
use sea_orm::DatabaseConnection;
use webauthn_rs::Webauthn;

use crate::{config::Config, mail::Mailer, rate_limit::RateLimiter};

#[derive(Clone)]
pub struct AppState {
    pub db: DatabaseConnection,
    pub git_commit: String,
    /// `TRUST_PROXY`: take the client IP from `X-Forwarded-For` (see `rate_limit::ClientIp`).
    pub trust_proxy: bool,
    /// Per-IP limit on `POST /v1/waitlist`.
    pub waitlist_limiter: Arc<RateLimiter>,
    /// Per-IP limit shared by `POST /auth/link` and `POST /auth/magic`.
    pub magic_link_limiter: Arc<RateLimiter>,
    /// Per-IP limit on every password and passkey sign-in attempt.
    pub login_limiter: Arc<RateLimiter>,
    /// Per-email budget of failed `/auth/login` passwords (signed-in failures are charged too); a
    /// success clears it. Never refuses a signed-in request.
    pub login_email_limiter: Arc<RateLimiter<String>>,
    /// Per-user budget of failed password checks by a signed-in user (`PUT /auth/password`, the
    /// confirm on passkey registration). Keyed by user id, so outside traffic cannot drain it.
    pub account_limiter: Arc<RateLimiter<uuid::Uuid>>,
    /// Per-user burst limit on `POST /v1/team/invitations`, each of which sends an email. The
    /// daily caps live in Postgres (`rate_limit::daily_allow`).
    pub invitation_limiter: Arc<RateLimiter<uuid::Uuid>>,
    pub brain_url: String,
    pub web_url: String,
    /// `CORS_ORIGINS`: the web app's origins, the only ones that may call its state-changing
    /// `/auth` endpoints.
    pub web_origins: Vec<HeaderValue>,
    pub webauthn: Arc<Webauthn>,
    pub registration_disabled: bool,
    pub mailer: Mailer,
}

impl AppState {
    pub fn new(db: DatabaseConnection, config: &Config) -> Self {
        Self {
            db,
            git_commit: config.git_commit.clone(),
            trust_proxy: config.trust_proxy,
            waitlist_limiter: Arc::new(RateLimiter::per_minute(10)),
            magic_link_limiter: Arc::new(RateLimiter::per_minute(5)),
            login_limiter: Arc::new(RateLimiter::per_minute(10)),
            login_email_limiter: Arc::new(RateLimiter::per_minute(5)),
            account_limiter: Arc::new(RateLimiter::per_minute(5)),
            invitation_limiter: Arc::new(RateLimiter::per_minute(10)),
            brain_url: config.brain_url.clone(),
            web_url: config.web_url.clone(),
            web_origins: config.cors_origins.clone(),
            webauthn: config.webauthn.clone(),
            registration_disabled: config.registration_disabled,
            mailer: config.mailer.clone(),
        }
    }
}
