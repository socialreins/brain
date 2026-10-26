use axum::{extract::State, routing::get, Json, Router};
use serde_json::{json, Value};

use crate::state::AppState;

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn version(State(app): State<AppState>) -> Json<Value> {
    Json(json!({"commit": app.git_commit}))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
}
