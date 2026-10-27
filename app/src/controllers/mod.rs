//! Routes: `/health`, `/version`, `/auth/*`, `/openapi.json`, and the documented `/v1` API.
pub mod auth;
pub mod health;
pub mod waitlist;

use axum::{
    extract::DefaultBodyLimit,
    http::{header, Method},
    routing::get,
    Json, Router,
};
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    trace::TraceLayer,
};
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;

use crate::{config::Config, state::AppState};

/// Largest `/v1` and `/auth` request body; a waitlist signup is well under 1 KB.
const MAX_BODY_BYTES: usize = 4096;

#[derive(OpenApi)]
#[openapi(
    info(title = "SocialReins brain", version = "0.1.0"),
    servers((url = "/v1"))
)]
struct ApiDoc;

/// Every `/v1` route, with paths relative to the `/v1` server URL.
fn api() -> OpenApiRouter<AppState> {
    let mut doc = ApiDoc::openapi();
    // utoipa fills these from Cargo metadata, which has none to give.
    doc.info.description = None;
    doc.info.license = None;
    OpenApiRouter::with_openapi(doc).merge(waitlist::api())
}

/// The request span. Logs the path only (no query), with the token in `/auth/magic/{token}`
/// replaced, so a sign-in link never reaches the log.
fn span(req: &axum::http::Request<axum::body::Body>) -> tracing::Span {
    let path = req.uri().path();
    let path = if path.starts_with("/auth/magic/") {
        "/auth/magic/{token}"
    } else {
        path
    };
    tracing::debug_span!("request", method = %req.method(), path)
}

pub fn router(state: AppState, config: &Config) -> Router {
    let (v1, doc) = api().split_for_parts();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(config.cors_origins.clone()))
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE])
        // The web app sends the session cookie on cross-origin calls to `/auth/*`.
        .allow_credentials(true);
    Router::new()
        .merge(health::routes())
        .merge(auth::routes().layer(DefaultBodyLimit::max(MAX_BODY_BYTES)))
        .nest("/v1", v1.layer(DefaultBodyLimit::max(MAX_BODY_BYTES)))
        .route("/openapi.json", get(move || async move { Json(doc) }))
        .layer(cors)
        .layer(TraceLayer::new_for_http().make_span_with(span))
        .with_state(state)
}
