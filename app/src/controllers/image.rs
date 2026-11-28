//! `GET /v1/public/image?url=<percent-encoded>`: a public page's image, fetched by brain for the
//! visitor. A post's media can link to any host, and a public page used to load it in the
//! reader's browser, so the author's host saw every visitor. Now the browser asks brain and brain
//! fetches the image through the same guarded client as a download (`Media::public_image`): a
//! public https URL only, at most 8 seconds, 5 MB, an image by header and by its first bytes, and
//! no cookie or auth header of anyone's on the way out. It is a proxy, not a store: nothing is
//! saved, the bytes go straight back. Limited per IP (`image_limiter`).
use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::waitlist::error;
use crate::{media::ImageFail, rate_limit::ClientIp, state::AppState};

/// Longest URL served: brain's own media links and the author's share links stay under it.
const MAX_URL_CHARS: usize = 2048;

#[derive(Deserialize, ToSchema)]
pub struct ImageQuery {
    /// The image's public https URL, percent-encoded.
    pub url: String,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn refuse(fail: ImageFail) -> Response {
    match fail {
        ImageFail::Blocked => reply(StatusCode::BAD_REQUEST, "invalid_url"),
        ImageFail::NotImage => reply(StatusCode::UNPROCESSABLE_ENTITY, "not_an_image"),
        ImageFail::TooLarge => reply(StatusCode::PAYLOAD_TOO_LARGE, "image_too_large"),
        ImageFail::TimedOut => reply(StatusCode::GATEWAY_TIMEOUT, "upstream_timeout"),
        ImageFail::Http(_) | ImageFail::Unreachable => {
            reply(StatusCode::BAD_GATEWAY, "upstream_unreachable")
        }
    }
}

/// Public: the image at `url`, fetched by brain and served from it. `400 invalid_url` for anything
/// but a public https URL (a redirect to one included), `422 not_an_image`, `413 image_too_large`,
/// `429 rate_limited`, `502` or `504` when the origin could not be read.
#[utoipa::path(get, path = "/public/image", tag = "posts",
    params(("url" = String, Query, description = "The image's public https URL, percent-encoded")),
    responses((status = 200, description = "The image's bytes, with the origin's Content-Type",
            body = Vec<u8>, content_type = "image/png",
            headers(("Cache-Control" = String, description = "`public, max-age=3600, immutable`"),
                ("X-Content-Type-Options" = String, description = "`nosniff`"))),
        (status = 400, body = super::waitlist::Error),
        (status = 413, body = super::waitlist::Error),
        (status = 422, body = super::waitlist::Error),
        (status = 429, body = super::waitlist::Error),
        (status = 502, body = super::waitlist::Error),
        (status = 504, body = super::waitlist::Error)))]
async fn image(
    State(app): State<AppState>,
    ClientIp(ip): ClientIp,
    Query(q): Query<ImageQuery>,
) -> Response {
    if q.url.chars().count() > MAX_URL_CHARS {
        return reply(StatusCode::BAD_REQUEST, "invalid_url");
    }
    if !app.image_limiter.allow(&app.db, ip).await {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    match app.media.public_image(&q.url).await {
        Ok(image) => (
            [
                (header::CONTENT_TYPE, image.content_type),
                (
                    header::CACHE_CONTROL,
                    "public, max-age=3600, immutable".to_owned(),
                ),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
            ],
            image.bytes,
        )
            .into_response(),
        Err(fail) => refuse(fail),
    }
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(image))
}
