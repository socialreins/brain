use std::sync::Arc;

use axum::http::HeaderValue;
use sea_orm::DatabaseConnection;
use socialreins_core::crypto::Cipher;
use webauthn_rs::Webauthn;

use crate::{config::Config, connectors::bluesky::Bluesky, mail::Mailer, rate_limit::RateLimiter};

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
    /// Per-IP limit on every passkey and Google sign-in attempt.
    pub login_limiter: Arc<RateLimiter>,
    /// Per-user burst limit on `POST /v1/team/invitations`, each of which sends an email. The
    /// daily caps live in Postgres (`rate_limit::daily_allow`).
    pub invitation_limiter: Arc<RateLimiter<uuid::Uuid>>,
    /// Per-user burst limit on `POST /v1/connections/bluesky`, each of which tries a password
    /// on a Bluesky server. Per IP it shares `login_limiter`; daily caps live in Postgres.
    pub connect_limiter: Arc<RateLimiter<uuid::Uuid>>,
    /// Per-user limit on publish-now and take-down calls, each of which calls a platform.
    pub post_limiter: Arc<RateLimiter<uuid::Uuid>>,
    /// Per-token limit on every request made with an API token.
    pub api_token_limiter: Arc<RateLimiter<uuid::Uuid>>,
    pub brain_url: String,
    pub web_url: String,
    /// `CORS_ORIGINS`: the web app's origins, the only ones that may call its state-changing
    /// `/auth` endpoints.
    pub web_origins: Vec<HeaderValue>,
    pub webauthn: Arc<Webauthn>,
    pub registration_disabled: bool,
    pub owner_email: Option<String>,
    pub mailer: Mailer,
    pub cipher: Arc<Cipher>,
    pub bluesky: Bluesky,
    pub providers: Vec<(&'static str, bool)>,
    pub google: Option<crate::connectors::google::Google>,
    pub webhooks: crate::webhooks::Webhooks,
    pub turnstile: Option<crate::turnstile::Turnstile>,
    pub contact_to: Option<String>,
    /// Per-IP limit on `POST /v1/contact`, each of which sends an email.
    pub contact_limiter: Arc<RateLimiter>,
    /// `ADMIN_EMAILS`: users who may read `/v1/admin/*`.
    pub admin_emails: Vec<String>,
    /// `REVIEW_LOGIN_TOKEN`: `GET /review-login/{token}`.
    pub review_login_token: Option<String>,
    /// `GET /v1/waitlist/count`'s last answer and when it was read from the database.
    pub waitlist_count: Arc<std::sync::Mutex<Option<(std::time::Instant, u64)>>>,
    /// Post media while a post publishes (`media.rs`).
    pub media: crate::media::Media,
    pub threads: crate::connectors::threads::Threads,
    pub linkedin: crate::connectors::linkedin::LinkedIn,
    /// The Meta app for connecting, and its two publishers.
    pub meta: crate::connectors::meta::Meta,
    pub facebook: crate::connectors::meta::Facebook,
    pub instagram: crate::connectors::meta::Instagram,
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
            invitation_limiter: Arc::new(RateLimiter::per_minute(10)),
            connect_limiter: Arc::new(RateLimiter::per_minute(5)),
            post_limiter: Arc::new(RateLimiter::per_minute(10)),
            api_token_limiter: Arc::new(RateLimiter::per_minute(120)),
            brain_url: config.brain_url.clone(),
            web_url: config.web_url.clone(),
            web_origins: config.cors_origins.clone(),
            webauthn: config.webauthn.clone(),
            registration_disabled: config.registration_disabled,
            owner_email: config.owner_email.clone(),
            mailer: config.mailer.clone(),
            cipher: config.cipher.clone(),
            bluesky: config.bluesky.clone(),
            providers: config.providers.clone(),
            google: config.google.clone(),
            webhooks: crate::webhooks::Webhooks::default(),
            turnstile: config.turnstile.clone(),
            contact_to: config.contact_to.clone(),
            contact_limiter: Arc::new(RateLimiter::new(
                5,
                std::time::Duration::from_secs(3600),
                crate::rate_limit::MAX_KEYS,
            )),
            admin_emails: config.admin_emails.clone(),
            review_login_token: config.review_login_token.clone(),
            waitlist_count: Arc::default(),
            media: config.media.clone(),
            threads: config.threads.clone(),
            linkedin: config.linkedin.clone(),
            meta: config.meta.clone(),
            facebook: crate::connectors::meta::Facebook(config.meta.clone()),
            instagram: crate::connectors::meta::Instagram(config.meta.clone()),
        }
    }
}
