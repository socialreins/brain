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
    /// Per-IP limit on password and passkey sign-in, and on `PUT /auth/password`.
    pub login_limiter: Arc<RateLimiter>,
    /// Per-email limit on the same.
    pub login_email_limiter: Arc<RateLimiter<String>>,
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
            brain_url: config.brain_url.clone(),
            web_url: config.web_url.clone(),
            web_origins: config.cors_origins.clone(),
            webauthn: config.webauthn.clone(),
            registration_disabled: config.registration_disabled,
            mailer: config.mailer.clone(),
        }
    }
}
