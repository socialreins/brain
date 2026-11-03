//! Media storage. A post's media URLs (Google Drive share links too) are downloaded once, at
//! publish time, into brain's own storage: local disk at `MEDIA_DIR`, or an S3-compatible bucket
//! (`config.rs`). Each file is streamed with a size cap and typed by its magic bytes, and the
//! publisher reads it from there. Storage is temporary: a post's files are deleted once it
//! settles, and an hourly sweep deletes what is left. Downloads resolve names through
//! `PublicOnly` and follow at most 3 redirects, each to a public https URL.
use std::{collections::HashMap, sync::Arc, time::Duration};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use tokio::time::Instant;

use object_store::{path::Path, ObjectStore, WriteMultipart};
use reqwest::{header::CONTENT_TYPE, StatusCode, Url};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement, TransactionTrait, Value,
};
use uuid::Uuid;

use crate::{connectors::bluesky::PublicOnly, state::AppState, webhooks::public_url};

/// The largest file brain stores, whatever the platform.
pub const MAX_BYTES: usize = 100_000_000;
/// Files older than this whose post no longer waits to publish are deleted by the sweep.
pub const SWEEP_DAYS: i32 = 7;
const MAX_REDIRECTS: usize = 3;
/// Longest all of a post's downloads may take together, so a slow server cannot hold a tick.
pub const INGEST_DEADLINE: Duration = Duration::from_secs(60);
/// Downloads of one post running at once.
const PARALLEL: usize = 4;
/// Bytes needed to tell every stored type apart.
const SNIFF_LEN: usize = 12;
const SWEEP_LOCK: i64 = 0x004d_4544_4941_5357;

/// The store and the client that downloads into it.
#[derive(Clone)]
pub struct Media {
    pub store: Arc<dyn ObjectStore>,
    client: reqwest::Client,
    allow_private: bool,
    /// `INGEST_DEADLINE`; tests shorten it.
    pub deadline: Duration,
}

/// Why a file was not stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fail {
    /// Not a public https URL, or it redirected to one that is not.
    Blocked,
    /// Google Drive answered with a page, not the file: it is not shared publicly.
    NotShared,
    Http(u16),
    Unreachable,
    TooLarge,
    /// Not a JPEG, PNG, WebP, GIF, MP4, MOV or PDF.
    Unsupported,
    Storage,
    /// The post's downloads ran past `INGEST_DEADLINE` before this one was stored.
    TimedOut,
}

/// A stored file.
#[derive(Clone, Debug)]
pub struct Stored {
    pub key: String,
    pub content_type: String,
}

impl Media {
    /// `allow_private` (tests only, never read from the environment) also downloads over plain
    /// http from private address literals. Names still resolve to public addresses only.
    pub fn new(store: Arc<dyn ObjectStore>, allow_private: bool) -> Self {
        let redirects = reqwest::redirect::Policy::custom(move |a| {
            if a.previous().len() > MAX_REDIRECTS {
                a.error("too many redirects")
            } else if public_url(a.url(), allow_private) {
                a.follow()
            } else {
                a.error("redirect to a URL that is not public")
            }
        });
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(120))
            .redirect(redirects)
            .dns_resolver(Arc::new(PublicOnly))
            .user_agent("SocialReins")
            .build()
            .expect("the media http client builds");
        Self {
            store,
            client,
            allow_private,
            deadline: INGEST_DEADLINE,
        }
    }

    /// `ingest` of every URL, `PARALLEL` at a time, all within `deadline`; a URL not stored by
    /// then is `TimedOut`, its upload aborted.
    pub async fn ingest_all(
        &self,
        db: &DatabaseConnection,
        post: Uuid,
        urls: &[String],
        cap: usize,
    ) -> HashMap<String, Result<Stored, Fail>> {
        let mut left: Vec<String> = Vec::new();
        for url in urls {
            if !left.contains(url) {
                left.push(url.clone());
            }
        }
        left.reverse();
        let until = Instant::now() + self.deadline;
        let mut found = HashMap::new();
        let mut running = tokio::task::JoinSet::new();
        loop {
            while running.len() < PARALLEL {
                let Some(url) = left.pop() else { break };
                let (media, db) = (self.clone(), db.clone());
                running.spawn(async move {
                    let got = media.ingest(&db, post, &url, cap, until).await;
                    (url, got)
                });
            }
            // Each download stops itself at `until`, so this ends then too.
            match running.join_next().await {
                Some(Ok((url, got))) => {
                    found.insert(url, got);
                }
                Some(Err(e)) => tracing::error!(error = %e, post = %post, "media task failed"),
                None => break,
            }
        }
        for url in urls {
            found.entry(url.clone()).or_insert(Err(Fail::Storage));
        }
        found
    }

    /// The stored copy of `url` for `post`: the one an earlier try saved, else downloaded now,
    /// at most `cap` bytes (and never over `MAX_BYTES`), by `until`. A row whose file is gone is
    /// a miss: it is deleted and the URL downloaded again.
    pub async fn ingest(
        &self,
        db: &DatabaseConnection,
        post: Uuid,
        url: &str,
        cap: usize,
        until: Instant,
    ) -> Result<Stored, Fail> {
        let db_err = |e: DbErr| {
            tracing::warn!(error = %e, post = %post, "post media row failed");
            Fail::Storage
        };
        let found = db
            .query_one_raw(sql(
                "SELECT storage_key, content_type FROM post_media \
                 WHERE post_id = $1 AND source_url = $2",
                vec![post.into(), url.into()],
            ))
            .await
            .map_err(db_err)?;
        if let Some(row) = found {
            let s = stored(&row).map_err(db_err)?;
            match self.store.head(&Path::from(s.key.as_str())).await {
                Err(object_store::Error::NotFound { .. }) => {
                    let stmt = sql(
                        "DELETE FROM post_media WHERE post_id = $1 AND source_url = $2",
                        vec![post.into(), url.into()],
                    );
                    db.execute_raw(stmt).await.map_err(db_err)?;
                }
                _ => return Ok(s),
            }
        }
        let got = self.download(post, url, cap.min(MAX_BYTES), until).await;
        let (key, content_type, size) = got?;
        // A run that stored the same URL first wins; this copy is then deleted.
        let saved = db
            .query_one_raw(sql(
                "INSERT INTO post_media (id, post_id, source_url, storage_key, content_type, size) \
                 VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (post_id, source_url) DO UPDATE SET post_id = EXCLUDED.post_id \
                 RETURNING storage_key, content_type",
                vec![
                    Uuid::now_v7().into(),
                    post.into(),
                    url.into(),
                    key.clone().into(),
                    content_type.into(),
                    (size as i64).into(),
                ],
            ))
            .await;
        let kept = match saved {
            Ok(Some(row)) => stored(&row).map_err(db_err),
            Ok(None) => Err(Fail::Storage),
            Err(e) => Err(db_err(e)),
        };
        if kept.as_ref().map_or(true, |s| s.key != key) {
            self.delete(&key).await;
        }
        kept
    }

    /// Streams `url` (a Drive share link as its direct download) into a new object; returns its
    /// key, type and size.
    async fn download(
        &self,
        post: Uuid,
        url: &str,
        cap: usize,
        until: Instant,
    ) -> Result<(String, &'static str, usize), Fail> {
        let drive = drive_url(url);
        let from = Url::parse(drive.as_deref().unwrap_or(url)).map_err(|_| Fail::Blocked)?;
        if !public_url(&from, self.allow_private) {
            return Err(Fail::Blocked);
        }
        let sent = tokio::time::timeout_at(until, self.client.get(from).send());
        let mut res = sent.await.map_err(|_| Fail::TimedOut)?.map_err(|e| {
            let fail = if e.is_redirect() || blocked(&e) {
                Fail::Blocked
            } else {
                Fail::Unreachable
            };
            tracing::warn!(error = %e.without_url(), post = %post, "media download failed");
            fail
        })?;
        if res.status() != StatusCode::OK {
            return Err(Fail::Http(res.status().as_u16()));
        }
        if res.content_length().is_some_and(|n| n > cap as u64) {
            return Err(Fail::TooLarge);
        }
        let html = res
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().starts_with("text/html"));
        let read = |e: reqwest::Error| {
            tracing::warn!(error = %e.without_url(), post = %post, "media download failed");
            Fail::Unreachable
        };
        let mut head = Vec::new();
        while head.len() < SNIFF_LEN {
            let chunk = tokio::time::timeout_at(until, res.chunk()).await;
            match chunk.map_err(|_| Fail::TimedOut)?.map_err(read)? {
                Some(chunk) => head.extend_from_slice(&chunk),
                None => break,
            }
        }
        let Some(content_type) = sniff(&head) else {
            // Drive answers a file it will not hand out with a sign-in or error page.
            return Err(if drive.is_some() && html {
                Fail::NotShared
            } else {
                Fail::Unsupported
            });
        };
        let mut size = head.len();
        if size > cap {
            return Err(Fail::TooLarge);
        }
        let key = format!("posts/{post}/{}", Uuid::now_v7());
        let upload = self
            .store
            .put_multipart(&Path::from(key.as_str()))
            .await
            .map_err(storage)?;
        let mut file = WriteMultipart::new(upload);
        file.write(&head);
        let streamed = async {
            while let Some(chunk) = res.chunk().await.map_err(read)? {
                size += chunk.len();
                if size > cap {
                    return Err(Fail::TooLarge);
                }
                file.wait_for_capacity(4).await.map_err(storage)?;
                file.write(&chunk);
            }
            Ok(())
        };
        // Past the deadline the upload is aborted, so no partial file stays behind.
        let streamed = tokio::time::timeout_at(until, streamed)
            .await
            .unwrap_or(Err(Fail::TimedOut));
        match streamed {
            Ok(()) => file.finish().await.map_err(storage)?,
            Err(e) => {
                let _ = file.abort().await;
                return Err(e);
            }
        };
        Ok((key, content_type, size))
    }

    /// The bytes of a stored file.
    pub async fn read(&self, key: &str) -> Result<Vec<u8>, object_store::Error> {
        let got = self.store.get(&Path::from(key)).await?;
        Ok(got.bytes().await?.to_vec())
    }

    /// Deletes a stored file; true when it is gone. Errors are logged, never returned.
    async fn delete(&self, key: &str) -> bool {
        match self.store.delete(&Path::from(key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => true,
            Err(e) => {
                tracing::warn!(error = %e, key, "stored media delete failed");
                false
            }
        }
    }

    /// Deletes every stored file under `posts/<post>/` older than `days` of posts that no longer
    /// wait to publish (settled, deleted or gone), whether or not a row names it, then their
    /// rows; only `post`'s when given. Storage errors are logged and left for the next sweep.
    /// Returns how many files went.
    pub async fn release(
        &self,
        db: &DatabaseConnection,
        post: Option<Uuid>,
        days: i32,
    ) -> Result<u64, DbErr> {
        let posts: Vec<Path> = match post {
            Some(id) => vec![Path::from(format!("posts/{id}"))],
            None => match self
                .store
                .list_with_delimiter(Some(&Path::from("posts")))
                .await
            {
                Ok(list) => list.common_prefixes,
                Err(e) => {
                    tracing::warn!(error = %e, "stored media list failed");
                    Vec::new()
                }
            },
        };
        let cutoff = chrono::Utc::now() - chrono::Duration::days(days.into());
        let mut gone = 0;
        for prefix in posts {
            let Some(id) = prefix.filename().and_then(|f| f.parse::<Uuid>().ok()) else {
                continue;
            };
            let waiting = db
                .query_one_raw(sql(
                    "SELECT 1 AS ok FROM posts WHERE id = $1 \
                     AND status IN ('draft', 'scheduled', 'queued')",
                    vec![id.into()],
                ))
                .await?;
            if waiting.is_some() {
                continue;
            }
            let files = match self.store.list_with_delimiter(Some(&prefix)).await {
                Ok(list) => list.objects,
                Err(e) => {
                    tracing::warn!(error = %e, post = %id, "stored media list failed");
                    continue;
                }
            };
            for file in files.into_iter().filter(|f| f.last_modified <= cutoff) {
                if self.delete(file.location.as_ref()).await {
                    gone += 1;
                }
            }
        }
        // Rows only point at files; one whose file is still up is gone from the store next time.
        let stmt = sql(
            "DELETE FROM post_media m WHERE ($1::uuid IS NULL OR m.post_id = $1) \
             AND m.created_at <= now() - make_interval(days => $2) \
             AND NOT EXISTS (SELECT 1 FROM posts p WHERE p.id = m.post_id \
                             AND p.status IN ('draft', 'scheduled', 'queued'))",
            vec![post.into(), days.into()],
        );
        db.execute_raw(stmt).await?;
        Ok(gone)
    }

    /// `release` for one post, right after it settles or is deleted; errors are only logged.
    pub async fn release_post(&self, db: &DatabaseConnection, post: Uuid) {
        if let Err(e) = self.release(db, Some(post), 0).await {
            tracing::warn!(error = %e, post = %post, "post media release failed");
        }
    }
}

/// The hourly sweep: `release` of files older than `SWEEP_DAYS`, by one instance at a time.
pub async fn sweep(app: &AppState) -> Result<u64, DbErr> {
    let lock = app.db.begin().await?;
    let row = lock
        .query_one_raw(sql(
            "SELECT pg_try_advisory_xact_lock($1) AS ok",
            vec![SWEEP_LOCK.into()],
        ))
        .await?;
    let locked: bool = match row {
        Some(r) => r.try_get("", "ok")?,
        None => false,
    };
    if !locked {
        return Ok(0);
    }
    let gone = app.media.release(&app.db, None, SWEEP_DAYS).await?;
    lock.commit().await?;
    Ok(gone)
}

/// The unguessable brain URL of a stored file, for platforms that download media themselves
/// (Threads): its storage key sealed with `ENCRYPTION_KEY`, plus an extension for its type. It
/// serves the file only while the file is stored, which ends once its post settles.
pub fn link(app: &AppState, file: &Stored) -> String {
    let sealed = URL_SAFE_NO_PAD.encode(app.cipher.encrypt(file.key.as_bytes()));
    let ext = match file.content_type.as_str() {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        _ => "bin",
    };
    format!("{}/media/{sealed}.{ext}", app.brain_url)
}

/// The storage key a `link` names, when brain sealed it and it is a post file.
pub fn unseal(app: &AppState, name: &str) -> Option<String> {
    let token = name.split('.').next()?;
    let sealed = URL_SAFE_NO_PAD.decode(token).ok()?;
    let key = String::from_utf8(app.cipher.decrypt(&sealed)?).ok()?;
    key.starts_with("posts/").then_some(key)
}

fn sql(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

fn stored(row: &sea_orm::QueryResult) -> Result<Stored, DbErr> {
    Ok(Stored {
        key: row.try_get("", "storage_key")?,
        content_type: row.try_get("", "content_type")?,
    })
}

fn storage(e: object_store::Error) -> Fail {
    tracing::warn!(error = %e, "media storage write failed");
    Fail::Storage
}

/// The request never got an answer because `PublicOnly` refused the host.
fn blocked(e: &reqwest::Error) -> bool {
    let mut cause: Option<&dyn std::error::Error> = Some(e);
    while let Some(c) = cause {
        if c.to_string()
            .contains(crate::connectors::bluesky::NOT_PUBLIC)
        {
            return true;
        }
        cause = c.source();
    }
    false
}

/// A Google Drive share link (`/file/d/<id>/view`, `open?id=<id>`, `uc?id=<id>`) as its direct
/// download, past the virus-scan page large files get (`confirm=t`); `None` for other URLs.
pub fn drive_url(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;
    if u.scheme() != "https" || !matches!(u.host_str()?, "drive.google.com" | "docs.google.com") {
        return None;
    }
    let segments: Vec<&str> = u.path_segments()?.collect();
    let id = match segments.as_slice() {
        ["file", "d", id, ..] => (*id).to_owned(),
        ["open"] | ["uc"] => u.query_pairs().find(|(k, _)| k == "id")?.1.into_owned(),
        _ => return None,
    };
    let plain = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    (!id.is_empty() && id.chars().all(plain)).then(|| {
        format!("https://drive.usercontent.google.com/download?id={id}&export=download&confirm=t")
    })
}

/// The type of a file by its first bytes: JPEG, PNG, GIF, WebP, MP4, MOV or PDF.
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    if head.len() >= SNIFF_LEN && &head[4..8] == b"ftyp" {
        // HEIC and AVIF are `ftyp` boxes too; only video brands count.
        return match &head[8..12] {
            b"qt  " => Some("video/quicktime"),
            b"isom" | b"iso2" | b"iso4" | b"iso5" | b"iso6" | b"mp41" | b"mp42" | b"avc1"
            | b"M4V " | b"dash" => Some("video/mp4"),
            _ => None,
        };
    }
    match head {
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, ..] => Some("image/png"),
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        [b'%', b'P', b'D', b'F', b'-', ..] => Some("application/pdf"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_share_links_become_direct_downloads() {
        let direct = |id: &str| {
            Some(format!(
                "https://drive.usercontent.google.com/download?id={id}&export=download&confirm=t"
            ))
        };
        for link in [
            "https://drive.google.com/file/d/1AbC-d_9/view?usp=sharing",
            "https://drive.google.com/file/d/1AbC-d_9/view",
            "https://drive.google.com/file/d/1AbC-d_9",
            "https://drive.google.com/open?id=1AbC-d_9",
            "https://drive.google.com/uc?id=1AbC-d_9&export=download",
            "https://drive.google.com/uc?export=download&id=1AbC-d_9",
            "https://docs.google.com/uc?id=1AbC-d_9",
        ] {
            assert_eq!(drive_url(link), direct("1AbC-d_9"), "{link}");
        }
        for other in [
            "https://example.com/file/d/1AbC/view",
            "http://drive.google.com/file/d/1AbC/view",
            "https://drive.google.com/drive/folders/1AbC",
            "https://drive.google.com/open?id=",
            "https://drive.google.com/open?id=a%26b",
            "https://drive.google.com.evil.test/uc?id=1AbC",
        ] {
            assert_eq!(drive_url(other), None, "{other}");
        }
    }

    #[test]
    fn types_come_from_magic_bytes() {
        let cases: [(&[u8], Option<&str>); 10] = [
            (b"\xFF\xD8\xFF\xE0\0\x10JFIF\0\x01", Some("image/jpeg")),
            (b"\x89PNG\r\n\x1a\n\0\0\0\x0d", Some("image/png")),
            (b"GIF89a\x01\0\x01\0\0\0", Some("image/gif")),
            (b"RIFF\x24\0\0\0WEBPVP8 ", Some("image/webp")),
            (b"\0\0\0\x18ftypmp42\0\0", Some("video/mp4")),
            (b"\0\0\0\x14ftypqt  \0\0", Some("video/quicktime")),
            (b"%PDF-1.7\n%\xE2\xE3", Some("application/pdf")),
            (b"\0\0\0\x18ftypheic\0\0", None),
            (b"<!DOCTYPE html>", None),
            (b"\xFF\xD8", None),
        ];
        for (head, want) in cases {
            assert_eq!(sniff(head), want, "{head:?}");
        }
    }

    fn soon() -> Instant {
        Instant::now() + INGEST_DEADLINE
    }

    fn png(len: usize) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n".to_vec();
        b.resize(len, 7);
        b
    }

    /// Serves `/ok.png` (100 KB), `/page` (HTML) and `/hop`, a redirect to `/ok.png`.
    async fn server() -> String {
        use axum::{response::Redirect, routing::get, Router};
        let app = Router::new()
            .route(
                "/ok.png",
                get(|| async { ([("content-type", "image/png")], png(100_000)) }),
            )
            .route(
                "/page",
                get(|| async { ([("content-type", "text/html")], "<html>".repeat(9)) }),
            )
            .route("/hop", get(|| async { Redirect::temporary("/ok.png") }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        base
    }

    #[tokio::test]
    async fn downloads_stream_into_the_store_with_a_cap_and_a_type() {
        let base = server().await;
        let media = Media::new(Arc::new(object_store::memory::InMemory::new()), true);
        let post = Uuid::now_v7();
        for path in ["/ok.png", "/hop"] {
            let (key, ty, size) = media
                .download(post, &format!("{base}{path}"), MAX_BYTES, soon())
                .await
                .unwrap();
            assert_eq!((ty, size), ("image/png", 100_000));
            assert!(key.starts_with(&format!("posts/{post}/")));
            assert_eq!(media.read(&key).await.unwrap(), png(100_000));
        }
        let got = media
            .download(post, &format!("{base}/ok.png"), 99_999, soon())
            .await;
        assert_eq!(got.unwrap_err(), Fail::TooLarge);
        let got = media
            .download(post, &format!("{base}/page"), MAX_BYTES, soon())
            .await;
        assert_eq!(got.unwrap_err(), Fail::Unsupported);
    }

    #[tokio::test]
    async fn private_addresses_are_never_fetched() {
        let base = server().await;
        let media = Media::new(Arc::new(object_store::memory::InMemory::new()), false);
        let post = Uuid::now_v7();
        for url in [
            format!("{base}/ok.png"),
            base.replace("http://", "https://") + "/ok.png",
            "https://localhost/ok.png".to_owned(),
            "https://10.0.0.1/ok.png".to_owned(),
            "https://[::1]/ok.png".to_owned(),
            "https://user@example.com/ok.png".to_owned(),
        ] {
            assert_eq!(
                media
                    .download(post, &url, MAX_BYTES, soon())
                    .await
                    .unwrap_err(),
                Fail::Blocked,
                "{url}"
            );
        }
    }
}
