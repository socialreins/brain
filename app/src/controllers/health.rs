use std::time::Duration;

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use sea_orm::ConnectionTrait;
use serde_json::{json, Value};

use crate::state::AppState;

/// Longest the database may take to answer `/health`: a pool or database that is slower than
/// this is down as far as a monitor is concerned.
const DB_WAIT: Duration = Duration::from_secs(3);

/// Up only while Postgres answers a query, so an uptime monitor sees a dead database as 503.
/// Railway reads it at deploy time only (`railway.json`), so a database outage never restarts brain.
async fn health(State(app): State<AppState>) -> (StatusCode, Json<Value>) {
    match tokio::time::timeout(DB_WAIT, app.db.execute_unprepared("SELECT 1")).await {
        Ok(Ok(_)) => (StatusCode::OK, Json(json!({"status": "ok"}))),
        Ok(Err(e)) => {
            tracing::error!(error = %e, "health check: database query failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"status": "error"})),
            )
        }
        Err(_) => {
            tracing::error!("health check: database did not answer in {DB_WAIT:?}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"status": "error"})),
            )
        }
    }
}

async fn version(State(app): State<AppState>) -> Json<Value> {
    Json(json!({"commit": app.git_commit}))
}

// The SocialReins mark, so brain's own pages (the sign-in confirm page) show it in the tab.
const FAVICON_ICO: &[u8] = include_bytes!("../../assets/favicon.ico");
const ICON_SVG: &[u8] = include_bytes!("../../assets/icon.svg");

fn asset(content_type: &'static str, body: &'static [u8]) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        body,
    )
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route(
            "/favicon.ico",
            get(|| async { asset("image/x-icon", FAVICON_ICO) }),
        )
        .route(
            "/icon.svg",
            get(|| async { asset("image/svg+xml", ICON_SVG) }),
        )
}
