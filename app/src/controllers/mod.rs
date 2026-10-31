//! Routes: `/health`, `/version`, `/auth/*`, `/openapi.json`, the documented `/v1` API
//! (waitlist, teams, API tokens, webhooks, connections, social sets, posts, lists, inboxes,
//! automations), and `/mcp` (see `crate::mcp`).
pub mod api_tokens;
pub mod auth;
pub mod automations;
pub mod connections;
pub mod google;
pub mod health;
pub mod idempotency;
pub mod inbox;
pub mod lists;
pub mod passkeys;
pub mod posts;
pub mod social_sets;
pub mod teams;
pub mod waitlist;
pub mod webhooks;

use axum::{
    extract::DefaultBodyLimit,
    http::{header, Method},
    middleware::from_fn_with_state,
    routing::get,
    Json, Router,
};
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    trace::TraceLayer,
};
use utoipa::{
    openapi::security::{ApiKey, ApiKeyValue, Http, HttpAuthScheme, SecurityScheme},
    OpenApi,
};
use utoipa_axum::router::OpenApiRouter;

use crate::{config::Config, mcp, state::AppState};

/// Largest `/v1` and `/auth` request body; a waitlist signup is well under 1 KB.
pub(crate) const MAX_BODY_BYTES: usize = 4096;
/// Passkey bodies carry an attestation or assertion, which can run past 4 KiB.
const MAX_PASSKEY_BODY_BYTES: usize = 16 * 1024;
const MAX_AUTOMATION_BODY_BYTES: usize = 32 * 1024;

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
    OpenApiRouter::with_openapi(doc)
        .merge(waitlist::api())
        .merge(teams::api())
        .merge(api_tokens::api())
        .merge(webhooks::api())
        .merge(connections::api())
        .merge(social_sets::api())
        .merge(posts::api())
        .merge(lists::api())
        .merge(inbox::api())
        // Two DM texts of 1000 graphemes can pass 4 KiB.
        .merge(automations::api().layer(DefaultBodyLimit::max(MAX_AUTOMATION_BODY_BYTES)))
}

/// The request span. Logs the path only (no query), with the token in `/auth/magic/{token}` and
/// `/v1/invitations/{token}` replaced, so a sign-in or invitation link never reaches the log.
/// Headers are not logged, so neither is an API token's `Authorization` header.
fn span(req: &axum::http::Request<axum::body::Body>) -> tracing::Span {
    let path = req.uri().path();
    let path = if path.starts_with("/auth/magic/") {
        "/auth/magic/{token}"
    } else if path.starts_with("/v1/invitations/") {
        "/v1/invitations/{token}"
    } else {
        path
    };
    tracing::debug_span!("request", method = %req.method(), path)
}

pub fn router(state: AppState, config: &Config) -> Router {
    let (v1, mut doc) = api().split_for_parts();
    let components = doc.components.get_or_insert_default();
    components.add_security_scheme(
        "session",
        SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::new(auth::SESSION_COOKIE))),
    );
    components.add_security_scheme(
        "api_token",
        SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
    );
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(config.cors_origins.clone()))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([header::CONTENT_TYPE])
        // The web app sends the session cookie on cross-origin calls to `/auth/*`.
        .allow_credentials(true);
    Router::new()
        .merge(health::routes())
        .merge(
            auth::routes()
                .merge(google::routes())
                .layer(DefaultBodyLimit::max(MAX_BODY_BYTES)),
        )
        .merge(passkeys::routes().layer(DefaultBodyLimit::max(MAX_PASSKEY_BODY_BYTES)))
        .nest(
            "/v1",
            v1.layer(from_fn_with_state(state.clone(), idempotency::layer))
                .layer(DefaultBodyLimit::max(MAX_BODY_BYTES)),
        )
        .route("/openapi.json", get(move || async move { Json(doc) }))
        .merge(
            Router::new()
                .nest_service("/mcp", mcp::service(&state))
                .route_layer(from_fn_with_state(state.clone(), mcp::auth::layer)),
        )
        .layer(cors)
        .layer(TraceLayer::new_for_http().make_span_with(span))
        .with_state(state)
}
