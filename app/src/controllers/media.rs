//! `GET /media/{name}`: a stored post file at its unguessable link (`media::link`), for platforms
//! that download media themselves (Threads). It answers only while the file is stored: once its
//! post settles the file and its row go, and the link answers `404`.
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use sea_orm::{ConnectionTrait, DbBackend, Statement};

use crate::{media, state::AppState};

async fn serve(State(app): State<AppState>, Path(name): Path<String>) -> Response {
    let not_found = || StatusCode::NOT_FOUND.into_response();
    let Some(key) = media::unseal(&app, &name) else {
        return not_found();
    };
    let row = app
        .db
        .query_one_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            "SELECT content_type FROM post_media WHERE storage_key = $1 LIMIT 1",
            [key.clone().into()],
        ))
        .await;
    let content_type: String = match row.map(|r| r.map(|r| r.try_get("", "content_type"))) {
        Ok(Some(Ok(t))) => t,
        Ok(_) => return not_found(),
        Err(e) => {
            tracing::error!(error = %e, "post media lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let got = match app
        .media
        .store
        .get(&object_store::path::Path::from(key))
        .await
    {
        Ok(got) => got,
        Err(object_store::Error::NotFound { .. }) => return not_found(),
        Err(e) => {
            tracing::warn!(error = %e, "stored media read failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let size = got.meta.size.to_string();
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_LENGTH, size),
            (header::CACHE_CONTROL, "no-store".into()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
        ],
        Body::from_stream(got.into_stream()),
    )
        .into_response()
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/media/{name}", get(serve))
}
