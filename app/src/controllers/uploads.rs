//! `POST /v1/uploads`: a file from the user's device for a post's media. The body is the raw
//! file, streamed into the media store (`Media::put_stream`), never held whole; its type comes
//! from its first bytes, not from any header. The answer's `url` goes in a post's `media` (or
//! a thread part's, or `cover`) like any link: saving a post checks that each upload link is the
//! team's, and publishing copies the file inside the store instead of downloading it. The link
//! also serves the file (`GET /media/{name}`) for the composer's preview. An upload lives a day,
//! then as long as a post of the team that may still publish names it: one not published yet,
//! or published with a failed target to retry (`media::sweep`).
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
    media::{Fail, Stored, MAX_STILL_BYTES},
    state::AppState,
};

/// Most uploads a team keeps at once.
pub const MAX_UPLOADS: i64 = 200;
/// Most bytes of uploads a team keeps at once: 10 GiB.
pub const MAX_UPLOAD_BYTES: i64 = 10 * 1024 * 1024 * 1024;
/// Most uploads of a team in progress at once.
pub const MAX_IN_PROGRESS: i64 = 3;

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

/// The team's uploads: how many, their bytes (an upload in progress counts the most it may
/// take), and how many are in progress.
async fn usage(db: &impl ConnectionTrait, team: Uuid) -> Result<(i64, i64, i64), DbErr> {
    let stmt = sql(
        "SELECT count(*) AS n, coalesce(sum(size), 0)::bigint AS bytes, \
         count(*) FILTER (WHERE pending) AS busy FROM media_uploads WHERE team_id = $1",
        vec![team.into()],
    );
    match db.query_one_raw(stmt).await? {
        Some(r) => Ok((
            r.try_get("", "n")?,
            r.try_get("", "bytes")?,
            r.try_get("", "busy")?,
        )),
        None => Ok((0, 0, 0)),
    }
}

/// An upload's `pending` row, deleted with its file unless the upload finished: on an error,
/// or when the client goes away and the request is dropped.
struct Pending {
    app: AppState,
    id: Uuid,
    done: bool,
}

impl Pending {
    /// Deletes the row and the file now.
    async fn clear(mut self) {
        // Spawned, so it finishes even if the request is dropped while it runs.
        let (app, id) = (self.app.clone(), self.id);
        let cleared = tokio::spawn(async move { release_pending(&app, id).await });
        self.done = true;
        let _ = cleared.await;
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.done {
            let (app, id) = (self.app.clone(), self.id);
            tokio::spawn(async move { release_pending(&app, id).await });
        }
    }
}

/// Deletes upload `id` while it is still `pending`, then its file; one that finished meanwhile
/// (its last `UPDATE` committed) keeps both. A partial file is the multipart upload's to abort,
/// and the sweep's after `UPLOAD_TEMP_AGE`.
pub async fn release_pending(app: &AppState, id: Uuid) {
    let stmt = sql(
        "DELETE FROM media_uploads WHERE id = $1 AND pending RETURNING storage_key",
        vec![id.into()],
    );
    match app.db.query_one_raw(stmt).await {
        Ok(Some(row)) => {
            if let Ok(key) = row.try_get::<String>("", "storage_key") {
                let _ = app.media.store.delete(&key.as_str().into()).await;
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, upload = %id, "pending upload not cleared"),
    }
}

/// Uploads a file (the raw request body) for a post's media: a JPEG, PNG, WebP or GIF image or
/// a PDF up to 100 MB, or an MP4 or MOV video up to `MEDIA_MAX_BYTES`, told by its bytes. Any
/// member, and any token that may write. `422 media_type_not_supported` for another type (or an
/// empty body), `413 media_too_large` over the size, `409 too_many_uploads` when the team keeps
/// 200 uploads, or would pass 10 GiB counting `MEDIA_MAX_BYTES` for this one until it is in.
/// `429 uploads_in_progress` while 3 of the team's uploads are running, `429 rate_limited` past
/// 60 a minute. `402 plan_limit` with billing on and no live plan. Each platform's own media
/// rules apply when a post is saved. Not idempotent: an `Idempotency-Key` on a body over 64 KiB
/// is `413`.
#[utoipa::path(post, path = "/uploads", tag = "posts",
    security(("session" = []), ("api_token" = [])),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses((status = 201, body = Upload), (status = 401, body = Error),
        (status = 402, body = Error), (status = 403, body = Error), (status = 409, body = Error),
        (status = 413, body = Error), (status = 422, body = Error), (status = 429, body = Error)))]
async fn create(State(app): State<AppState>, a: Actor, body: Body) -> Response {
    if let Some(res) = super::billing::needs_plan(&app, a.team.id).await {
        return res;
    }
    if !app.upload_limiter.hit(a.team.id) {
        return reply(StatusCode::TOO_MANY_REQUESTS, "rate_limited");
    }
    let id = Uuid::now_v7();
    // Made here from ids only, so no part of the request reaches the path.
    let key = format!("uploads/{}/{id}", a.team.id);
    // Held in the quota until the file is in, so parallel uploads cannot all stream to disk.
    let reserve = app.media.max_bytes.max(MAX_STILL_BYTES) as i64;
    // Before the reservation commits, so a request dropped during the commit still clears it.
    let mut pending = Pending {
        app: app.clone(),
        id,
        done: false,
    };
    let reserved = async {
        let txn = app.db.begin().await?;
        lock(&txn, "uploads", a.team.id).await?;
        let (n, bytes, busy) = usage(&txn, a.team.id).await?;
        if busy >= MAX_IN_PROGRESS {
            return Ok(Err(reply(
                StatusCode::TOO_MANY_REQUESTS,
                "uploads_in_progress",
            )));
        }
        if n >= MAX_UPLOADS || bytes + reserve > MAX_UPLOAD_BYTES {
            return Ok(Err(reply(StatusCode::CONFLICT, "too_many_uploads")));
        }
        txn.execute_raw(sql(
            "INSERT INTO media_uploads (id, team_id, storage_key, url, content_type, size, \
             pending, created_by) VALUES ($1, $2, $3, '', '', $4, true, $5)",
            vec![
                id.into(),
                a.team.id.into(),
                key.clone().into(),
                reserve.into(),
                a.user_id.into(),
            ],
        ))
        .await?;
        txn.commit().await?;
        Ok::<_, DbErr>(Ok(()))
    };
    match reserved.await {
        Ok(Ok(())) => {}
        Ok(Err(res)) => {
            pending.done = true;
            return res;
        }
        Err(e) => {
            pending.clear().await;
            return internal(e, "upload reserve");
        }
    }
    let res = stream(&app, id, key, body).await;
    if res.status() == StatusCode::CREATED {
        pending.done = true;
    } else {
        pending.clear().await;
    }
    res
}

/// Streams the file into the store and marks its row done: the `201`, or the error.
async fn stream(app: &AppState, id: Uuid, key: String, body: Body) -> Response {
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
        key,
        content_type: content_type.into(),
        size: size as i64,
    };
    let Some(url) = app.media.upload_link(&file) else {
        return reply(StatusCode::INTERNAL_SERVER_ERROR, "internal");
    };
    let done = app
        .db
        .execute_raw(sql(
            "UPDATE media_uploads SET url = $2, content_type = $3, size = $4, pending = false \
             WHERE id = $1 AND pending",
            vec![
                id.into(),
                url.clone().into(),
                content_type.into(),
                file.size.into(),
            ],
        ))
        .await;
    match done {
        Ok(r) if r.rows_affected() == 1 => {
            let body = Upload {
                id,
                url,
                content_type: content_type.into(),
                size: file.size,
            };
            (StatusCode::CREATED, Json(body)).into_response()
        }
        // Swept as lost while it streamed.
        Ok(_) => reply(StatusCode::BAD_REQUEST, "upload_interrupted"),
        Err(e) => internal(e, "upload insert"),
    }
}

/// The first upload link among `urls` that is not an upload of `team` as brain gave it: another
/// team's, gone, or another spelling of the team's own (a query, another host, an escape), so a
/// post only ever names an upload by its exact link, which the sweep matches. In a transaction the uploads found stay locked `FOR SHARE` until it ends, so the
/// sweep cannot delete one under a post being saved (`media::release_uploads`).
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
            "SELECT 1 AS ok FROM media_uploads WHERE storage_key = $1 AND team_id = $2 \
             AND url = $3 AND NOT pending FOR SHARE",
            vec![key.into(), team.into(), (*url).into()],
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
