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
/// Bytes a probe asks for: enough for `animated` to see a WebP's flags, and a PNG's `acTL`
/// unless a long chunk (a colour profile, say) comes first.
const PROBE_LEN: usize = 512;
/// Image conversions (`to_jpeg`, `fit`) running at once in the process, so the decode and resize
/// buffers of many posts (up to a few hundred MB each) queue instead of piling up.
static CONVERTING: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
const SWEEP_LOCK: i64 = 0x004d_4544_4941_5357;

/// The store and the client that downloads into it.
#[derive(Clone)]
pub struct Media {
    pub store: Arc<dyn ObjectStore>,
    client: reqwest::Client,
    allow_private: bool,
    /// `INGEST_DEADLINE`; tests shorten it.
    pub deadline: Duration,
    /// `PROBE_DEADLINE`; tests shorten it.
    pub probe_deadline: Duration,
    /// Tests only: a probe of a URL starting with `.0` asks `.1` instead (a mock on loopback,
    /// which saved media URLs, always https, cannot name).
    pub probe_rewrite: Option<(String, String)>,
}

/// Longest the probes of one post's media may take together when it is saved. A probe not
/// answered by then is unknown, and an unknown file is checked at publish only.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(4);

/// What a probe learned of a media URL: its type from its first bytes, and its size when the
/// server said (`Content-Range` or `Content-Length`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Probed {
    pub content_type: &'static str,
    pub size: Option<u64>,
    /// An animated WebP or PNG, as far as the first bytes show (`animated`).
    pub animated: bool,
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
    pub size: i64,
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
            probe_deadline: PROBE_DEADLINE,
            probe_rewrite: None,
        }
    }

    /// `probe` of every URL, `PARALLEL` at a time, all within `probe_deadline`; only the URLs
    /// that answered are in the map.
    pub async fn probe_all(&self, urls: &[String]) -> HashMap<String, Probed> {
        let mut left: Vec<String> = Vec::new();
        for url in urls {
            if !left.contains(url) {
                left.push(url.clone());
            }
        }
        let until = Instant::now() + self.probe_deadline;
        let mut found = HashMap::new();
        let mut running = tokio::task::JoinSet::new();
        loop {
            while running.len() < PARALLEL {
                let Some(url) = left.pop() else { break };
                let media = self.clone();
                running.spawn(async move {
                    let got = tokio::time::timeout_at(until, media.probe(&url)).await;
                    (url, got.ok().flatten())
                });
            }
            match running.join_next().await {
                Some(Ok((url, Some(got)))) => {
                    found.insert(url, got);
                }
                Some(_) => {}
                None => break,
            }
        }
        found
    }

    /// The type and size of `url` (a Drive share link as its direct download) from a ranged GET
    /// of its first bytes, through the same `PublicOnly` client and checks as `download`. `None`
    /// when there is no clear answer: blocked, unreachable, not 200 or 206, or bytes of no type
    /// brain stores (a sign-in page, say). Nothing is stored.
    pub async fn probe(&self, url: &str) -> Option<Probed> {
        let drive = drive_url(url);
        let mut url = drive.unwrap_or_else(|| url.to_owned());
        if let Some((from, to)) = self.probe_rewrite.as_ref().filter(|_| self.allow_private) {
            if let Some(rest) = url.strip_prefix(from.as_str()) {
                url = format!("{to}{rest}");
            }
        }
        let from = Url::parse(&url).ok()?;
        if !public_url(&from, self.allow_private) {
            return None;
        }
        let range = format!("bytes=0-{}", PROBE_LEN - 1);
        let mut res = self
            .client
            .get(from)
            .header(reqwest::header::RANGE, range)
            .send()
            .await
            .ok()?;
        let size = match res.status() {
            // `bytes 0-511/12345`; `*` for an unknown total.
            StatusCode::PARTIAL_CONTENT => res
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.rsplit('/').next())
                .and_then(|n| n.parse().ok()),
            StatusCode::OK => res.content_length(),
            _ => return None,
        };
        let mut head = Vec::new();
        while head.len() < PROBE_LEN {
            match res.chunk().await.ok()? {
                Some(chunk) => head.extend_from_slice(&chunk),
                None => break,
            }
        }
        Some(Probed {
            content_type: sniff(&head)?,
            size,
            animated: animated(&head),
        })
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
                "SELECT storage_key, content_type, size FROM post_media \
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
                 RETURNING storage_key, content_type, size",
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

    /// `file`, the stored copy of `url`, swapped for its JPEG (`to_jpeg`): the row points at the
    /// new file and the old one is deleted. `file` as it was when it does not decode or storage
    /// fails, so the platform's type check refuses it as before.
    pub async fn as_jpeg(
        &self,
        db: &DatabaseConnection,
        post: Uuid,
        url: &str,
        file: Stored,
    ) -> Stored {
        self.swap(db, post, url, file, to_jpeg).await
    }

    /// `file`, the stored copy of `url`, swapped for a JPEG of at most `max` bytes (`fit`).
    /// `file` as it was when it cannot be made to fit, so the size check refuses it.
    pub async fn fitted(
        &self,
        db: &DatabaseConnection,
        (post, url): (Uuid, &str),
        file: Stored,
        (max, side): (usize, u32),
    ) -> Stored {
        self.swap(db, post, url, file, move |b| fit(b, max, side))
            .await
    }

    /// `file` swapped for the JPEG `convert` makes of its bytes, off the async runtime, at most
    /// two at a time (`CONVERTING`). The wait for a turn is bounded by the work ahead of it: each
    /// conversion is CPU only, of at most `MAX_PIXELS` and a fixed number of encodes. No clock
    /// runs against it: the downloads and their deadline are over, and no target is claimed yet,
    /// so no publisher timeout or stuck-target time has started. A post that waits so long that a
    /// tick takes it for stuck is only run again, which is safe: the row swap below is guarded by
    /// the old key, and each target is claimed once.
    async fn swap(
        &self,
        db: &DatabaseConnection,
        post: Uuid,
        url: &str,
        file: Stored,
        convert: impl FnOnce(&[u8]) -> Option<Vec<u8>> + Send + 'static,
    ) -> Stored {
        // Taken before the bytes are read, so a waiting post holds none.
        let Ok(turn) = CONVERTING.acquire().await else {
            return file;
        };
        let bytes = match self.read(&file.key).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(error = %e, post = %post, "stored media read failed");
                return file;
            }
        };
        // The turn goes with the work, which runs on even if this future is dropped.
        let work = move || {
            let _turn = turn;
            convert(&bytes)
        };
        let Ok(Some(jpeg)) = tokio::task::spawn_blocking(work).await else {
            tracing::warn!(post = %post, "media image did not convert to JPEG");
            return file;
        };
        let key = format!("posts/{post}/{}", Uuid::now_v7());
        let size = jpeg.len() as i64;
        if let Err(e) = self.store.put(&Path::from(key.as_str()), jpeg.into()).await {
            storage(e);
            return file;
        }
        let swapped = db
            .execute_raw(sql(
                "UPDATE post_media SET storage_key = $3, content_type = 'image/jpeg', size = $4 \
                 WHERE post_id = $1 AND source_url = $2 AND storage_key = $5",
                vec![
                    post.into(),
                    url.into(),
                    key.clone().into(),
                    size.into(),
                    file.key.clone().into(),
                ],
            ))
            .await;
        match swapped {
            Ok(r) if r.rows_affected() == 1 => {
                self.delete(&file.key).await;
                Stored {
                    key,
                    content_type: "image/jpeg".into(),
                    size,
                }
            }
            other => {
                if let Err(e) = other {
                    tracing::warn!(error = %e, post = %post, "post media row failed");
                }
                self.delete(&key).await;
                file
            }
        }
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
        size: row.try_get("", "size")?,
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

/// Largest image `to_jpeg` decodes, so a small file cannot claim a huge canvas: its copies stay
/// within a few hundred MB.
const MAX_PIXELS: u64 = 25_000_000;

/// Whether an image is an animated WebP (the animation flag of its `VP8X` header) or an animated
/// PNG (an `acTL` chunk before the first `IDAT`), as far as `bytes` go: a head cut off before
/// the answer is `false`. Re-encoding one as a JPEG would keep its first frame only.
pub fn animated(bytes: &[u8]) -> bool {
    match sniff(bytes) {
        Some("image/webp") => {
            bytes.get(12..16) == Some(b"VP8X") && bytes.get(20).is_some_and(|f| f & 0x02 != 0)
        }
        Some("image/png") => {
            // Chunks follow the 8 byte signature: length, type, data, CRC.
            let mut at = 8usize;
            while let Some(chunk) = bytes.get(at..at.saturating_add(8)) {
                match &chunk[4..] {
                    b"acTL" => return true,
                    b"IDAT" => return false,
                    _ => {}
                }
                let len = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                at = at.saturating_add(len as usize).saturating_add(12);
            }
            false
        }
        _ => false,
    }
}

/// An image decoded to RGB at its own size, turned upright by its EXIF orientation (the JPEG
/// written from it carries none), any transparency laid on white, and its ICC colour profile
/// when that is an RGB one (a grey or CMYK profile would not match the RGB pixels). `None` when
/// it does not decode or is over `MAX_PIXELS`.
fn decode_rgb(bytes: &[u8]) -> Option<(image::RgbImage, Option<Vec<u8>>)> {
    use image::ImageDecoder;
    let mut decoder = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    let (w, h) = decoder.dimensions();
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return None;
    }
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let icc = decoder.icc_profile().ok().flatten();
    let icc = icc.filter(|p| p.get(16..20) == Some(b"RGB "));
    let mut decoded = image::DynamicImage::from_decoder(decoder).ok()?;
    decoded.apply_orientation(orientation);
    let rgb = if decoded.color().has_alpha() {
        let mut rgba = decoded.into_rgba8();
        for p in rgba.pixels_mut() {
            let a = u16::from(p[3]);
            for c in &mut p.0[..3] {
                *c = ((u16::from(*c) * a + 255 * (255 - a)) / 255) as u8;
            }
        }
        image::DynamicImage::ImageRgba8(rgba).into_rgb8()
    } else {
        decoded.into_rgb8()
    };
    Some((rgb, icc))
}

fn encode_jpeg(rgb: &image::RgbImage, quality: u8, icc: Option<&[u8]>) -> Option<Vec<u8>> {
    use image::ImageEncoder;
    let mut out = Vec::new();
    let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    if let Some(icc) = icc {
        encoder.set_icc_profile(icc.to_vec()).ok()?;
    }
    encoder.encode_image(rgb).ok()?;
    Some(out)
}

/// An image (PNG or WebP) as a JPEG at quality 90, at its own size, upright, any transparency
/// laid on white, its RGB colour profile kept; `None` when it does not decode or is over
/// `MAX_PIXELS`. An animated one becomes its first frame.
pub fn to_jpeg(bytes: &[u8]) -> Option<Vec<u8>> {
    let (rgb, icc) = decode_rgb(bytes)?;
    encode_jpeg(&rgb, 90, icc.as_deref())
}

/// JPEG qualities `fit` tries at the image's own size, best first.
const FIT_QUALITIES: [u8; 3] = [90, 80, 70];
/// The quality of a downscaled image, and how many sizes `fit` tries before giving up.
const FIT_SCALED_QUALITY: u8 = 80;
const FIT_SCALES: usize = 6;

/// A still image (JPEG, PNG or WebP) as a JPEG of at most `max` bytes with its longest side at
/// most `side` pixels, upright, any transparency laid on white. Quality drops first
/// (`FIT_QUALITIES`), then the image is scaled down, its shape kept, never up, at most
/// `FIT_SCALES` times. It aims at 90% of `max`. `None` when it does not decode, is animated
/// (`animated`), is over `MAX_PIXELS` or still does not fit.
pub fn fit(bytes: &[u8], max: usize, side: u32) -> Option<Vec<u8>> {
    use image::imageops::{resize, FilterType::Lanczos3};
    if animated(bytes) {
        return None;
    }
    let (full, icc) = decode_rgb(bytes)?;
    let (w, h) = full.dimensions();
    let target = max / 10 * 9;
    // The colour profile is kept unless it would take much of the room itself.
    let icc = icc.filter(|p| p.len() <= target / 4);
    let encode_jpeg = |rgb: &image::RgbImage, q: u8| encode_jpeg(rgb, q, icc.as_deref());
    let sized = |scale: f64| {
        let to = |v: u32| ((f64::from(v) * scale).round() as u32).max(1);
        resize(&full, to(w), to(h), Lanczos3)
    };
    // The share of its sides the image keeps: all of them unless it is longer than `side`.
    let mut scale = (f64::from(side) / f64::from(w.max(h))).min(1.0);
    let mut len = 0;
    if scale == 1.0 {
        for q in FIT_QUALITIES {
            let out = encode_jpeg(&full, q)?;
            if out.len() <= target {
                return Some(out);
            }
            if q == FIT_SCALED_QUALITY {
                len = out.len();
            }
        }
    }
    for _ in 0..FIT_SCALES {
        if len > 0 {
            // A JPEG's size follows its pixel count; a little under, so one step usually does.
            scale *= ((target as f64 / len as f64).sqrt() * 0.95).min(0.9);
        }
        let out = encode_jpeg(&sized(scale), FIT_SCALED_QUALITY)?;
        if out.len() <= target {
            return Some(out);
        }
        len = out.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_png_with_transparency_becomes_a_jpeg_on_white() {
        // Left half see-through, right half opaque red, blocks wide enough for JPEG's 8x8.
        let png = image::RgbaImage::from_fn(32, 16, |x, _| {
            if x < 16 {
                image::Rgba([0, 0, 0, 0])
            } else {
                image::Rgba([200, 10, 10, 255])
            }
        });
        let mut bytes = Vec::new();
        png.write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
        assert_eq!(sniff(&bytes), Some("image/png"));

        let jpeg = to_jpeg(&bytes).unwrap();
        assert_eq!(sniff(&jpeg), Some("image/jpeg"));
        let back = image::load_from_memory(&jpeg).unwrap().into_rgb8();
        assert_eq!(back.dimensions(), (32, 16));
        let near =
            |got: [u8; 3], want: [u8; 3]| got.iter().zip(want).all(|(g, w)| g.abs_diff(w) < 40);
        assert!(
            near(back.get_pixel(4, 8).0, [255, 255, 255]),
            "transparent is white"
        );
        assert!(
            near(back.get_pixel(28, 8).0, [200, 10, 10]),
            "opaque keeps its colour"
        );
        assert_eq!(to_jpeg(b"\x89PNG\r\n\x1a\n not really"), None);

        // A WebP the same way.
        let mut webp = Vec::new();
        png.write_to(
            &mut std::io::Cursor::new(&mut webp),
            image::ImageFormat::WebP,
        )
        .unwrap();
        assert_eq!(sniff(&webp), Some("image/webp"));
        let back = image::load_from_memory(&to_jpeg(&webp).unwrap())
            .unwrap()
            .into_rgb8();
        assert!(near(back.get_pixel(4, 8).0, [255, 255, 255]));

        // A canvas past the decode cap is refused, however small its file.
        let huge = image::GrayImage::new(5_001, 5_000);
        let mut bytes = Vec::new();
        huge.write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
        assert_eq!(to_jpeg(&bytes), None);
    }

    /// A PNG of noise, which no codec shrinks to nothing, with see-through pixels if `alpha`.
    fn noise_png(w: u32, h: u32, alpha: bool) -> Vec<u8> {
        let mut seed = 0x9E37_79B9u32;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed >> 8) as u8
        };
        let mut bytes = Vec::new();
        let to = &mut std::io::Cursor::new(&mut bytes);
        if alpha {
            image::RgbaImage::from_fn(w, h, |x, _| {
                image::Rgba([next(), next(), next(), if x < w / 2 { 0 } else { 255 }])
            })
            .write_to(to, image::ImageFormat::Png)
        } else {
            image::RgbImage::from_fn(w, h, |_, _| image::Rgb([next(), next(), next()]))
                .write_to(to, image::ImageFormat::Png)
        }
        .unwrap();
        bytes
    }

    #[test]
    fn an_image_over_the_limit_is_shrunk_to_fit() {
        // 1080 x 1350 of noise is over 4 MB as a PNG.
        let png = noise_png(1080, 1350, false);
        assert!(png.len() > 3_000_000, "{}", png.len());
        let out = fit(&png, 1_000_000, 2000).unwrap();
        assert!(out.len() <= 900_000, "{}", out.len());
        assert_eq!(sniff(&out), Some("image/jpeg"));
        let (w, h) = image::load_from_memory(&out)
            .unwrap()
            .into_rgb8()
            .dimensions();
        // Never larger, and the same shape.
        assert!(w <= 1080 && h <= 1350, "{w} x {h}");
        assert!(
            (f64::from(w) / f64::from(h) - 0.8).abs() < 0.01,
            "{w} x {h}"
        );
        // X's limit takes the same image at its own size.
        let out = fit(&png, 5_000_000, u32::MAX).unwrap();
        assert!(out.len() <= 4_500_000, "{}", out.len());
        let back = image::load_from_memory(&out).unwrap();
        assert_eq!((back.width(), back.height()), (1080, 1350));
        // What does not decode, a GIF among it (no decoder: it would lose its animation), and
        // a limit nothing reaches are left alone.
        assert_eq!(fit(b"GIF89a not really", 1_000_000, 2000), None);
        assert_eq!(fit(b"\x89PNG\r\n\x1a\n not really", 1_000_000, 2000), None);
        assert_eq!(fit(&noise_png(64, 64, false), 100, 2000), None);
    }

    #[test]
    fn a_very_large_image_is_scaled_down_in_shape() {
        let png = noise_png(3000, 1500, false);
        let out = fit(&png, 1_000_000, 2000).unwrap();
        assert!(out.len() <= 900_000, "{}", out.len());
        let (w, h) = image::load_from_memory(&out)
            .unwrap()
            .into_rgb8()
            .dimensions();
        assert!(w <= 2000, "{w} x {h}");
        assert!(
            (f64::from(w) / f64::from(h) - 2.0).abs() < 0.01,
            "{w} x {h}"
        );
        // Longer than the side but light: scaled to the side, no further.
        let mut flat = Vec::new();
        image::RgbImage::from_pixel(2500, 500, image::Rgb([10, 120, 200]))
            .write_to(
                &mut std::io::Cursor::new(&mut flat),
                image::ImageFormat::Png,
            )
            .unwrap();
        let out = fit(&flat, 1_000_000, 2000).unwrap();
        let back = image::load_from_memory(&out).unwrap();
        assert_eq!((back.width(), back.height()), (2000, 400));
    }

    #[test]
    fn a_shrunk_image_lays_its_transparency_on_white() {
        let out = fit(&noise_png(1000, 800, true), 500_000, 2000).unwrap();
        assert!(out.len() <= 450_000, "{}", out.len());
        let back = image::load_from_memory(&out).unwrap().into_rgb8();
        let (w, h) = back.dimensions();
        let p = back.get_pixel(w / 4, h / 2).0;
        assert!(p.iter().all(|c| *c > 235), "transparent is white: {p:?}");
    }

    /// The EXIF block of a camera that held the phone upright: orientation 6, the stored rows
    /// are turned a quarter clockwise to show.
    const EXIF_TURNED: &[u8] =
        b"MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0";

    /// 32 x 16, left half red and right half blue, with `EXIF_TURNED` and an ICC profile of
    /// colour space `space`, as a JPEG or a PNG.
    fn turned(format: image::ImageFormat, space: &[u8; 4]) -> (Vec<u8>, Vec<u8>) {
        use image::ImageEncoder;
        let img = image::RgbImage::from_fn(32, 16, |x, _| {
            image::Rgb(if x < 16 { [220, 20, 20] } else { [20, 20, 220] })
        });
        let mut icc = vec![0u8; 200];
        icc[16..20].copy_from_slice(space);
        let mut bytes = Vec::new();
        if format == image::ImageFormat::Png {
            let mut e = image::codecs::png::PngEncoder::new(&mut bytes);
            e.set_exif_metadata(EXIF_TURNED.to_vec()).unwrap();
            e.set_icc_profile(icc.clone()).unwrap();
            e.write_image(&img, 32, 16, image::ExtendedColorType::Rgb8)
        } else {
            let mut e = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 95);
            e.set_exif_metadata(EXIF_TURNED.to_vec()).unwrap();
            e.set_icc_profile(icc.clone()).unwrap();
            e.write_image(&img, 32, 16, image::ExtendedColorType::Rgb8)
        }
        .unwrap();
        (bytes, icc)
    }

    #[test]
    fn a_turned_photo_comes_out_upright_with_its_colour_profile() {
        use image::ImageDecoder;
        for format in [image::ImageFormat::Jpeg, image::ImageFormat::Png] {
            let (bytes, icc) = turned(format, b"RGB ");
            // Plain decoding ignores the orientation: this is the sideways image.
            let raw = image::load_from_memory(&bytes).unwrap();
            assert_eq!((raw.width(), raw.height()), (32, 16));
            for out in [
                to_jpeg(&bytes).unwrap(),
                fit(&bytes, 1_000_000, 2000).unwrap(),
            ] {
                let mut d =
                    image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(&out[..])).unwrap();
                assert_eq!(d.icc_profile().unwrap(), Some(icc.clone()), "{format:?}");
                // No orientation is written, so every reader shows these pixels as they are.
                assert_eq!(d.exif_metadata().unwrap(), None);
                let back = image::load_from_memory(&out).unwrap().into_rgb8();
                assert_eq!(back.dimensions(), (16, 32), "{format:?}");
                let (top, bottom) = (back.get_pixel(8, 6).0, back.get_pixel(8, 26).0);
                assert!(top[0] > 150 && top[2] < 100, "red on top: {top:?}");
                assert!(bottom[2] > 150 && bottom[0] < 100, "blue below: {bottom:?}");
            }
            // A profile that is not RGB is not carried over to the RGB JPEG.
            let (bytes, _) = turned(format, b"GRAY");
            let out = to_jpeg(&bytes).unwrap();
            let mut d =
                image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(&out[..])).unwrap();
            assert_eq!(d.icc_profile().unwrap(), None);
        }
    }

    #[test]
    fn an_animated_image_is_told_from_its_bytes_and_never_shrunk() {
        // An animated WebP: the animation flag of its VP8X header.
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X\x0a\0\0\0\x02\0\0\0".to_vec();
        webp.resize(2_000, 0);
        assert!(animated(&webp));
        assert_eq!(fit(&webp, 1_000, 2000), None);
        webp[20] = 0x10;
        assert!(!animated(&webp), "alpha only");
        assert!(!animated(&webp[..20]), "cut off before the flags");

        // An animated PNG: an acTL chunk between IHDR and the first IDAT.
        let png = noise_png(64, 64, false);
        assert!(!animated(&png));
        assert!(fit(&png, 1_000_000, 2000).is_some());
        let mut apng = png[..33].to_vec();
        apng.extend_from_slice(b"\0\0\0\x08acTL\0\0\0\x02\0\0\0\0\0\0\0\0");
        apng.extend_from_slice(&png[33..]);
        assert!(animated(&apng));
        assert!(animated(&apng[..41]), "the probe's first bytes are enough");
        assert!(!animated(&apng[..36]));
        assert_eq!(fit(&apng, 1_000_000, 2000), None);
        // An acTL after the pixels started is not an animation, and other types never are.
        let mut late = png.clone();
        late.extend_from_slice(b"\0\0\0\x08acTL\0\0\0\x02\0\0\0\0\0\0\0\0");
        assert!(!animated(&late));
        assert!(!animated(b"GIF89a not really"));
        assert!(!animated(b""));
    }

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
    async fn a_probe_reads_the_type_and_size_and_stores_nothing() {
        let base = server().await;
        let store = Arc::new(object_store::memory::InMemory::new());
        let media = Media::new(store.clone(), true);
        let want = Probed {
            content_type: "image/png",
            size: Some(100_000),
            animated: false,
        };
        for path in ["/ok.png", "/hop"] {
            assert_eq!(media.probe(&format!("{base}{path}")).await, Some(want));
        }
        assert_eq!(media.probe(&format!("{base}/page")).await, None);
        assert_eq!(media.probe(&format!("{base}/missing")).await, None);
        let urls = vec![format!("{base}/ok.png"), format!("{base}/page")];
        let found = media.probe_all(&urls).await;
        assert_eq!(found.len(), 1);
        assert_eq!(found[&urls[0]], want);
        assert!(store
            .list_with_delimiter(None)
            .await
            .unwrap()
            .objects
            .is_empty());
        // Never a private address outside tests.
        let strict = Media::new(store, false);
        assert_eq!(strict.probe("https://127.0.0.1/ok.png").await, None);
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
