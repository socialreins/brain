use std::sync::Arc;

use sea_orm::DatabaseConnection;

use crate::rate_limit::RateLimiter;

#[derive(Clone)]
pub struct AppState {
    pub db: DatabaseConnection,
    pub git_commit: String,
    /// Per-IP limit on `POST /v1/waitlist`.
    pub waitlist_limiter: Arc<RateLimiter>,
}

impl AppState {
    pub fn new(db: DatabaseConnection, git_commit: String) -> Self {
        Self {
            db,
            git_commit,
            waitlist_limiter: Arc::new(RateLimiter::per_minute(10)),
        }
    }
}
