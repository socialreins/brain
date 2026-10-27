use std::sync::Arc;

use sea_orm::DatabaseConnection;

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
    pub brain_url: String,
    pub web_url: String,
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
            brain_url: config.brain_url.clone(),
            web_url: config.web_url.clone(),
            registration_disabled: config.registration_disabled,
            mailer: config.mailer.clone(),
        }
    }
}
