//! `Idempotency-Key` on API token writes, so a client that lost a response can retry, even days
//! later, without posting twice. A `POST`, `PUT`, `PATCH` or `DELETE` made with an API token and
//! the header (1 to 255 printable ASCII characters, else `422 invalid_idempotency_key`) runs once
//! per key and team; tokens of one team share keys. A repeat with the same method, path, body and
//! token set (a set token's set, or none for a whole-team token) gets the stored status and body
//! with `Idempotent-Replayed: true`, or `409 idempotency_in_progress` while the first still runs;
//! with anything else, `422 idempotency_key_reused`. 5xx and 429 answers are not stored, so a retry runs again; every
//! other answer is. A first run left unfinished for 10 minutes (a crash) is run again. Keys live
//! 30 days. Sessions ignore the header. A request this layer answers itself counts once against
//! the token's rate limit, as the route would have.
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement, Value};
use socialreins_core::token;
use tracing::Instrument;
use uuid::Uuid;

use super::{
    api_tokens::{bearer_secret, find_live, limited},
    auth::internal,
    waitlist::error,
};
use crate::{models::api_tokens, state::AppState};

/// How long a key and its response are kept.
pub const KEEP_DAYS: i64 = 30;
/// A first run older than this is taken as lost to a crash.
const ABANDONED_MINUTES: i32 = 10;
/// The largest `/v1` body limit; bodies are buffered to fingerprint them.
const MAX_BODY: usize = super::MAX_GROUP_BODY_BYTES;

fn reply(status: StatusCode, code: &str) -> Response {
    error(status, code).into_response()
}

fn sql(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// 1 to 255 printable ASCII characters.
fn valid_key(v: &[u8]) -> Option<&str> {
    let ok = (1..=255).contains(&v.len()) && v.iter().all(|b| (0x20..=0x7e).contains(b));
    ok.then(|| std::str::from_utf8(v).ok()).flatten()
}

/// Whether an answer is kept for replay: a 5xx or 429 means the work did not (fully) happen.
fn kept(status: StatusCode) -> bool {
    !status.is_server_error() && status != StatusCode::TOO_MANY_REQUESTS
}

/// The `/v1` middleware.
pub async fn layer(State(app): State<AppState>, req: Request, next: Next) -> Response {
    let writes = matches!(
        *req.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    let headers = req.headers();
    let key = headers
        .get("idempotency-key")
        .map(|v| v.as_bytes().to_vec());
    let secret = headers
        .get(header::AUTHORIZATION)
        .and_then(bearer_secret)
        .map(str::to_owned);
    let (true, Some(key), Some(secret)) = (writes, key, secret) else {
        return next.run(req).await;
    };
    let token = match find_live(&app, &secret).await {
        Ok(Some(t)) => t,
        // The route answers `401`.
        Ok(None) => return next.run(req).await,
        Err(e) => return internal(e, "idempotency token lookup"),
    };
    // A request answered here skips the route, so it counts against the limit here, once.
    let answer = |res: Response| limited(&app, token.id).unwrap_or(res);
    let Some(key) = valid_key(&key).map(str::to_owned) else {
        return answer(reply(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_idempotency_key",
        ));
    };
    let (parts, body) = req.into_parts();
    let Ok(body) = to_bytes(body, MAX_BODY).await else {
        return answer(reply(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"));
    };
    let method = parts.method.as_str().to_owned();
    let path = parts.uri.path().to_owned();
    let fingerprint = token::digest(&[method.as_bytes(), path.as_bytes(), &body].join(&b'\n'));
    let team = token.team_id;
    let claimed = match claim(&app, &token, &key, &method, &path, &fingerprint).await {
        Ok(c) => c,
        Err(e) => return internal(e, "idempotency claim"),
    };
    if let Some(res) = claimed {
        return answer(res);
    }
    let req = Request::from_parts(parts, Body::from(body));
    // Spawned, so a client hanging up mid-request cannot cancel the run between its effect and
    // the stored response.
    let run = {
        let (app, key) = (app.clone(), key.clone());
        tokio::spawn(
            async move {
                let res = next.run(req).await;
                finish(&app, team, &key, res).await
            }
            .in_current_span(),
        )
    };
    match run.await {
        Ok(res) => res,
        Err(e) => {
            forget(&app, team, &key).await;
            internal(e, "idempotent request")
        }
    }
}

/// Takes the key for this request: `None` when it is ours to run, else the answer (a replay,
/// `409` or `422`).
async fn claim(
    app: &AppState,
    token: &api_tokens::Model,
    key: &str,
    method: &str,
    path: &str,
    fingerprint: &str,
) -> Result<Option<Response>, DbErr> {
    let (team, set) = (token.team_id, token.social_set_id);
    // Twice at most: once more after removing an abandoned run.
    for _ in 0..2 {
        let insert = sql(
            "INSERT INTO idempotency_keys \
             (team_id, key, method, path, fingerprint, token_id, social_set_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT DO NOTHING",
            vec![
                team.into(),
                key.into(),
                method.into(),
                path.into(),
                fingerprint.into(),
                token.id.into(),
                set.into(),
            ],
        );
        if app.db.execute_raw(insert).await?.rows_affected() == 1 {
            return Ok(None);
        }
        let found = sql(
            "SELECT fingerprint, social_set_id, status, response_status, response_body, content_type, \
             created_at < now() - make_interval(mins => $3) AS abandoned \
             FROM idempotency_keys WHERE team_id = $1 AND key = $2",
            vec![team.into(), key.into(), ABANDONED_MINUTES.into()],
        );
        // Gone since the insert (a 5xx or a purge): try again.
        let Some(row) = app.db.query_one_raw(found).await? else {
            continue;
        };
        // A set token never sees an answer given to another set (or to a whole-team token).
        if row.try_get::<String>("", "fingerprint")? != fingerprint
            || row.try_get::<Option<Uuid>>("", "social_set_id")? != set
        {
            let code = "idempotency_key_reused";
            return Ok(Some(reply(StatusCode::UNPROCESSABLE_ENTITY, code)));
        }
        if row.try_get::<String>("", "status")? == "done" {
            return Ok(Some(replay(
                row.try_get("", "response_status")?,
                row.try_get("", "response_body")?,
                row.try_get("", "content_type")?,
            )));
        }
        if !row.try_get::<bool>("", "abandoned")? {
            break;
        }
        let stale = sql(
            "DELETE FROM idempotency_keys WHERE team_id = $1 AND key = $2 \
             AND status = 'in_progress' AND created_at < now() - make_interval(mins => $3)",
            vec![team.into(), key.into(), ABANDONED_MINUTES.into()],
        );
        app.db.execute_raw(stale).await?;
    }
    let code = "idempotency_in_progress";
    Ok(Some(reply(StatusCode::CONFLICT, code)))
}

fn replay(status: i16, body: Option<Vec<u8>>, content_type: Option<String>) -> Response {
    let mut res = Response::new(Body::from(body.unwrap_or_default()));
    *res.status_mut() =
        StatusCode::from_u16(status as u16).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let headers = res.headers_mut();
    if let Some(ct) = content_type.and_then(|c| HeaderValue::from_str(&c).ok()) {
        headers.insert(header::CONTENT_TYPE, ct);
    }
    headers.insert("idempotent-replayed", HeaderValue::from_static("true"));
    res
}

/// Stores the route's answer for replay, or drops the key when the answer is not kept.
async fn finish(app: &AppState, team: Uuid, key: &str, res: Response) -> Response {
    if !kept(res.status()) {
        forget(app, team, key).await;
        return res;
    }
    let (parts, body) = res.into_parts();
    // Our own JSON answers, all small.
    let body = match to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            forget(app, team, key).await;
            return internal(e, "idempotent response body");
        }
    };
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let done = sql(
        "UPDATE idempotency_keys SET status = 'done', response_status = $3, \
         response_body = $4, content_type = $5 WHERE team_id = $1 AND key = $2",
        vec![
            team.into(),
            key.into(),
            (parts.status.as_u16() as i16).into(),
            body.to_vec().into(),
            content_type.into(),
        ],
    );
    // Not saved: the key stays in progress, so a retry gets `409` until it is abandoned.
    if let Err(e) = app.db.execute_raw(done).await {
        tracing::error!(error = %e, "idempotency response not saved");
    }
    Response::from_parts(parts, Body::from(body))
}

/// Drops the key, so a retry runs again. Logged, not answered: a key left behind is taken as
/// abandoned after 10 minutes.
async fn forget(app: &AppState, team: Uuid, key: &str) {
    let gone = sql(
        "DELETE FROM idempotency_keys WHERE team_id = $1 AND key = $2",
        vec![team.into(), key.into()],
    );
    if let Err(e) = app.db.execute_raw(gone).await {
        tracing::warn!(error = %e, "idempotency key not dropped");
    }
}

/// The purge of keys older than `KEEP_DAYS`, run hourly by `scheduler`.
pub fn purge_sql() -> String {
    format!("DELETE FROM idempotency_keys WHERE created_at < now() - interval '{KEEP_DAYS} days'")
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    #[test]
    fn keys_are_1_to_255_printable_ascii() {
        assert_eq!(super::valid_key(b"a"), Some("a"));
        assert!(super::valid_key(&[b'k'; 255]).is_some());
        assert!(super::valid_key(b"order 42/~").is_some());
        for bad in [&b""[..], &[b'k'; 256], b"tab\there", "caf\u{e9}".as_bytes()] {
            assert_eq!(super::valid_key(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn server_errors_and_rate_limits_are_not_kept() {
        for s in [200, 201, 204, 400, 404, 409, 422] {
            assert!(super::kept(StatusCode::from_u16(s).unwrap()), "{s}");
        }
        for s in [429, 500, 502, 503] {
            assert!(!super::kept(StatusCode::from_u16(s).unwrap()), "{s}");
        }
    }
}
