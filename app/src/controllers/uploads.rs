//! `POST /v1/uploads`: a file from the user's device for a post's media. The body is the raw
//! file, streamed into the media store (`Media::put_stream`), never held whole; its type comes
//! from its first bytes, not from any header. The answer's `url` goes in a post's `media` (or
//! a thread part's, or `cover`) like any link: saving a post checks that each upload link is the
//! team's, and publishing copies the file inside the store instead of downloading it. The link
//! also serves the file (`GET /media/{name}`) for the composer's preview. An upload lives a day,
//! then as long as a draft, scheduled or queued post of the team names it (`media::sweep`).
use axum::{
    body::Body,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement, TransactionTrait};
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::{
    api_tokens::Actor,
    auth::internal,
    library::lock,
    waitlist::{error, Error},
};
use crate::{
    media::{Fail, Stored},
    state::AppState,
};

/// Most uploads a team keeps at once.
pub const MAX_UPLOADS: i64 = 200;
/// Most bytes of uploads a team keeps at once: 10 GiB.
pub const MAX_UPLOAD_BYTES: i64 = 10 * 1024 * 1024 * 1024;

/// A stored upload.
#[derive(Serialize, ToSchema)]
pub struct Upload {
    pub id: Uuid,
    /// Brain's link to the file: put it in a post's `media` (or `cover`). It serves the file
    /// while it is kept.
    pub url: String,
    /// `image/jpeg`, `image/png`, `image/webp`, `image/gif`, `video/mp4`, `video/quicktime` or
    /// `application/pdf`, from the file's bytes.
    pub content_type: String,
    pub size: i64,
}

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn sql(sql: &str, values: Vec<sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// The team's uploads: how many, and their bytes.
async fn usage(db: &impl ConnectionTrait, team: Uuid) -> Result<(i64, i64), DbErr> {
    let stmt = sql(
        "SELECT count(*) AS n, coalesce(sum(size), 0)::bigint AS bytes FROM media_uploads \
         WHERE team_id = $1",
        vec![team.into()],
    );
    match db.query_one_raw(stmt).await? {
        Some(r) => Ok((r.try_get("", "n")?, r.try_get("", "bytes")?)),
        None => Ok((0, 0)),
    }
}

/// Uploads a file (the raw request body) for a post's media: a JPEG, PNG, WebP or GIF image or
/// a PDF up to 100 MB, or an MP4 or MOV video up to `MEDIA_MAX_BYTES`, told by its bytes. Any
/// member, and any token that may write. `422 media_type_not_supported` for another type (or an
/// empty body), `413 media_too_large` over the size, `409 too_many_uploads` when the team keeps
/// 200 uploads or 10 GiB already. Each platform's own media rules apply when a post is saved.
/// Not idempotent: an `Idempotency-Key` on a body over 64 KiB is `413`.
#[utoipa::path(post, path = "/uploads", tag = "posts",
    security(("session" = []), ("api_token" = [])),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses((status = 201, body = Upload), (status = 401, body = Error),
        (status = 403, body = Error), (status = 409, body = Error), (status = 413, body = Error),
        (status = 422, body = Error)))]
async fn create(State(app): State<AppState>, a: Actor, body: Body) -> Response {
    let full = || reply(StatusCode::CONFLICT, "too_many_uploads");
    match usage(&app.db, a.team.id).await {
        Ok((n, bytes)) if n >= MAX_UPLOADS || bytes >= MAX_UPLOAD_BYTES => return full(),
        Ok(_) => {}
        Err(e) => return internal(e, "upload usage"),
    }
    let id = Uuid::now_v7();
    // Made here from ids only, so no part of the request reaches the path.
    let key = format!("uploads/{}/{id}", a.team.id);
    let (content_type, size) = match app.media.put_stream(&key, body.into_data_stream()).await {
        Ok(stored) => stored,
        Err(Fail::Unsupported) => {
            return reply(StatusCode::UNPROCESSABLE_ENTITY, "media_type_not_supported")
        }
        Err(Fail::TooLarge | Fail::TooLargeStill) => {
            return reply(StatusCode::PAYLOAD_TOO_LARGE, "media_too_large")
        }
        Err(Fail::Stalled | Fail::Unreachable) => {
            return reply(StatusCode::BAD_REQUEST, "upload_interrupted")
        }
        Err(fail) => {
            tracing::warn!(?fail, "upload store failed");
            return reply(StatusCode::INTERNAL_SERVER_ERROR, "internal");
        }
    };
    let file = Stored {
        key: key.clone(),
        content_type: content_type.into(),
        size: size as i64,
    };
    let Some(url) = app.media.upload_link(&file) else {
        return reply(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    };
    let saved = async {
        let txn = app.db.begin().await?;
        lock(&txn, "uploads", a.team.id).await?;
        let (n, bytes) = usage(&txn, a.team.id).await?;
        if n >= MAX_UPLOADS || bytes + file.size > MAX_UPLOAD_BYTES {
            return Ok(false);
        }
        txn.execute_raw(sql(
            "INSERT INTO media_uploads (id, team_id, storage_key, url, content_type, size, \
             created_by) VALUES ($1, $2, $3, $4, $5, $6, $7)",
            vec![
                id.into(),
                a.team.id.into(),
                key.clone().into(),
                url.clone().into(),
                content_type.into(),
                file.size.into(),
                a.user_id.into(),
            ],
        ))
        .await?;
        txn.commit().await?;
        Ok::<_, DbErr>(true)
    };
    match saved.await {
        Ok(true) => {
            let body = Upload {
                id,
                url,
                content_type: content_type.into(),
                size: file.size,
            };
            (StatusCode::CREATED, Json(body)).into_response()
        }
        other => {
            let _ = app.media.store.delete(&key.as_str().into()).await;
            match other {
                Err(e) => internal(e, "upload insert"),
                _ => full(),
            }
        }
    }
}

/// The upload links among `urls` that are not uploads of `team`: another team's, or gone.
pub async fn foreign(
    app: &AppState,
    db: &impl ConnectionTrait,
    team: Uuid,
    urls: &[&str],
) -> Result<Option<String>, DbErr> {
    for url in urls {
        let Some(key) = app.media.upload_key(url) else {
            continue;
        };
        let stmt = sql(
            "SELECT 1 AS ok FROM media_uploads WHERE storage_key = $1 AND team_id = $2",
            vec![key.into(), team.into()],
        );
        if db.query_one_raw(stmt).await?.is_none() {
            return Ok(Some((*url).to_owned()));
        }
    }
    Ok(None)
}

pub fn api() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(create))
}
