use axum::{extract::State, http::header, response::IntoResponse, routing::get, Json, Router};
use serde_json::{json, Value};

use crate::state::AppState;

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
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
