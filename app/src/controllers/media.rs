//! `GET /media/{name}`: a stored post file at its unguessable link (`media::link`), for platforms
//! that download media themselves (Threads). It answers only while the file is stored: once its
//! post settles the file and its row go, and the link answers `404`. The file is streamed from
//! storage, never held whole, and a `Range` request (video pullers send them) gets just its
//! bytes: `206` with `Content-Range`, or `416` for a range past the end.
use std::ops::Range;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use sea_orm::{ConnectionTrait, DbBackend, Statement};

use crate::{media, state::AppState};

/// What a `Range` header asks of a file of `size` bytes: the bytes to send, `Err` when none of
/// them exist (`416`), and `None` when the whole file goes: no header, a unit other than
/// `bytes`, several ranges or bad syntax, which HTTP lets a server ignore.
fn byte_range(header: Option<&str>, size: u64) -> Option<Result<Range<u64>, ()>> {
    let (from, to) = header?.trim().strip_prefix("bytes=")?.split_once('-')?;
    let (from, to) = (from.trim(), to.trim());
    let range = if from.is_empty() {
        // The last `to` bytes.
        let last: u64 = to.parse().ok()?;
        if last == 0 {
            return Some(Err(()));
        }
        size.saturating_sub(last)..size
    } else {
        let start: u64 = from.parse().ok()?;
        let end = match to {
            "" => size,
            to => to
                .parse::<u64>()
                .ok()
                .filter(|end| *end >= start)?
                .saturating_add(1),
        };
        start..end.min(size)
    };
    Some(if range.start >= size {
        Err(())
    } else {
        Ok(range)
    })
}

async fn serve(
    State(app): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
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
    let (store, key) = (&app.media.store, object_store::path::Path::from(key));
    let failed = |e: object_store::Error| match e {
        object_store::Error::NotFound { .. } => not_found(),
        e => {
            tracing::warn!(error = %e, "stored media read failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    };
    // A plain request is one read, as before; only a ranged one asks for the size first.
    let asked = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let range = match asked {
        Some(asked) => match store.head(&key).await {
            Ok(meta) => byte_range(Some(asked), meta.size).map(|r| (r, meta.size)),
            Err(e) => return failed(e),
        },
        None => None,
    };
    let common = [
        (header::CONTENT_TYPE, content_type),
        (header::ACCEPT_RANGES, "bytes".into()),
        (header::CACHE_CONTROL, "no-store".into()),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
    ];
    let (status, range, size) = match range {
        None => (StatusCode::OK, None, 0),
        Some((Ok(range), size)) => (StatusCode::PARTIAL_CONTENT, Some(range), size),
        Some((Err(()), size)) => {
            let total = [(header::CONTENT_RANGE, format!("bytes */{size}"))];
            return (StatusCode::RANGE_NOT_SATISFIABLE, common, total).into_response();
        }
    };
    let options = object_store::GetOptions {
        range: range.clone().map(Into::into),
        ..Default::default()
    };
    let got = match store.get_opts(&key, options).await {
        Ok(got) => got,
        Err(e) => return failed(e),
    };
    let (length, total) = (got.range.end - got.range.start, got.meta.size);
    let mut res = (
        status,
        common,
        [(header::CONTENT_LENGTH, length.to_string())],
        Body::from_stream(got.into_stream()),
    )
        .into_response();
    if let Some(range) = range {
        let value = format!(
            "bytes {}-{}/{}",
            range.start,
            range.end - 1,
            total.max(size)
        );
        if let Ok(value) = value.parse() {
            res.headers_mut().insert(header::CONTENT_RANGE, value);
        }
    }
    res
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/media/{name}", get(serve))
}

#[cfg(test)]
mod tests {
    use super::byte_range;

    #[test]
    fn a_range_header_names_bytes_of_the_file_or_is_ignored() {
        let of = |h: &str| byte_range(Some(h), 1000);
        assert_eq!(of("bytes=0-99"), Some(Ok(0..100)));
        assert_eq!(of("bytes=0-0"), Some(Ok(0..1)));
        assert_eq!(of("bytes=500-"), Some(Ok(500..1000)));
        assert_eq!(of("bytes=-100"), Some(Ok(900..1000)));
        // Past the end is cut to the end, a suffix longer than the file is the whole file.
        assert_eq!(of("bytes=990-5000"), Some(Ok(990..1000)));
        assert_eq!(of("bytes=-5000"), Some(Ok(0..1000)));
        assert_eq!(of(" bytes=999-999 "), Some(Ok(999..1000)));
        // None of the bytes exist: 416.
        for none in [
            "bytes=1000-",
            "bytes=1000-2000",
            "bytes=-0",
            "bytes=99999999999-",
        ] {
            assert_eq!(of(none), Some(Err(())), "{none}");
        }
        assert_eq!(byte_range(Some("bytes=0-"), 0), Some(Err(())));
        // Not a single byte range: the whole file goes.
        for whole in [
            "bytes=5-2",
            "bytes=0-1,5-6",
            "items=0-5",
            "bytes=a-b",
            "bytes=",
            "0-5",
        ] {
            assert_eq!(of(whole), None, "{whole}");
        }
        assert_eq!(byte_range(None, 1000), None);
        assert_eq!(of("bytes=0-18446744073709551615"), Some(Ok(0..1000)));
    }
}
