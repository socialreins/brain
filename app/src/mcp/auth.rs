//! The bearer layer in front of `/mcp`: the same API tokens and per-token limit as `/v1`, never
//! a session cookie. Hands the tools the `Actor` through the request extensions.
use axum::{
    extract::{Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::Response,
};

use crate::{controllers::api_tokens::Actor, state::AppState};

pub async fn layer(State(app): State<AppState>, mut req: Request, next: Next) -> Response {
    let auth = req.headers().get(header::AUTHORIZATION).cloned();
    match Actor::from_bearer(&app, auth.as_ref()).await {
        Ok(a) => {
            req.extensions_mut().insert(a);
            next.run(req).await
        }
        Err(res) => {
            let mut res = *res;
            if res.status() == StatusCode::UNAUTHORIZED {
                let h = res.headers_mut();
                h.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            }
            res
        }
    }
}
