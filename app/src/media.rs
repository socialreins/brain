//! Media storage. A post's media URLs (Google Drive share links too) are downloaded once, at
//! publish time, into brain's own storage: local disk at `MEDIA_DIR`, or an S3-compatible bucket
//! (`config.rs`). Each file is streamed with a size cap and typed by its magic bytes, never held
//! whole in memory (`PART_BYTES`, `PARTS_IN_FLIGHT`), and the publisher reads it from there. Storage is temporary: a post's files are deleted once it
//! settles, and an hourly sweep deletes what is left. Downloads resolve names through
//! `PublicOnly` and follow at most 3 redirects, each to a public https URL.
use std::{collections::HashMap, sync::Arc, time::Duration};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use tokio::time::Instant;

use object_store::{
    path::Path, MultipartUpload, ObjectStore, PutPayload, PutResult, UploadPart, WriteMultipart,
};
use reqwest::{header::CONTENT_TYPE, StatusCode, Url};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement, TransactionTrait, Value,
};
use uuid::Uuid;

use crate::{
    connectors::bluesky::{PostError, PublicOnly},
    state::AppState,
    webhooks::public_url,
};

/// The largest video brain stores, whatever the platform, unless `MEDIA_MAX_BYTES` lowers it
/// (`Media::max_bytes`): 1 GiB.
pub const MAX_BYTES: usize = socialreins_core::post::MAX_VIDEO_BYTES as usize;
/// The largest file that is not a video (an image, a PDF): it is read whole to convert or
/// upload it, so it keeps the cap every file had before videos were streamed.
pub const MAX_STILL_BYTES: usize = socialreins_core::post::MAX_MEDIA_BYTES as usize;
/// Files older than this whose post no longer waits to publish are deleted by the sweep.
pub const SWEEP_DAYS: i32 = 7;
const MAX_REDIRECTS: usize = 3;
/// A download's base time: its answer must start within it, and the whole file may take it plus
/// its size at `MIN_SPEED` (the size the server said, else the cap), so 60 s for a small image
/// and about 3.5 minutes for 300 MB.
pub const INGEST_DEADLINE: Duration = Duration::from_secs(60);
/// The slowest download that still finishes in time, in bytes a second.
pub const MIN_SPEED: u64 = 2_000_000;
/// Longest all of a post's downloads may take together, whatever their sizes, so slow servers
/// cannot hold a publish slot. `publish::STUCK_MINUTES` is derived from it.
pub const INGEST_CEILING: Duration = Duration::from_secs(12 * 60);
/// Longest one request to an S3 bucket may take, retries included (`config::media_store`): a
/// read, or a copy, of the largest file at `MIN_SPEED`, about 10 minutes.
pub const STORE_TIMEOUT: Duration =
    Duration::from_secs(INGEST_DEADLINE.as_secs() + MAX_BYTES as u64 / MIN_SPEED);
/// A download that sends nothing for this long has stalled and fails at once.
pub const STALL: Duration = Duration::from_secs(30);
/// Bytes buffered before a part goes to storage (S3's and R2's smallest part), and parts being
/// written at once: a download holds at most `PART_BYTES * (PARTS_IN_FLIGHT + 1)`, about 15 MB,
/// however large the file.
pub const PART_BYTES: usize = 5 * 1024 * 1024;
pub const PARTS_IN_FLIGHT: usize = 2;
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
/// One turn per post group and media URL (`Media::ingest`): posts of a group publish at once,
/// and the first to take the turn downloads a video the others then copy in the store.
// shortcut: turns are per process, so two brain instances may each download a group's video;
// take a Postgres advisory lock instead if replicas ever publish the same group.
type Turn = tokio::sync::Mutex<()>;
type Turns = HashMap<(Uuid, String), std::sync::Weak<Turn>>;
static TURNS: std::sync::Mutex<Option<Turns>> = std::sync::Mutex::new(None);

fn turn_of(group: Uuid, url: &str) -> Arc<Turn> {
    let mut turns = TURNS.lock().unwrap_or_else(|e| e.into_inner());
    let turns = turns.get_or_insert_with(HashMap::new);
    turns.retain(|_, t| t.strong_count() > 0);
    let slot = turns.entry((group, url.to_owned())).or_default();
    slot.upgrade().unwrap_or_else(|| {
        let new = Arc::<Turn>::default();
        *slot = Arc::downgrade(&new);
        new
    })
}
/// A multipart upload that can still be aborted once `WriteMultipart::finish` has taken it:
/// `finish` leaves the upload open when its last part fails, and in a bucket the parts sent
/// before it would stay, and be billed, until a lifecycle rule removed them.
#[derive(Debug, Clone)]
struct Upload(Arc<tokio::sync::Mutex<Box<dyn MultipartUpload>>>);

#[async_trait::async_trait]
impl MultipartUpload for Upload {
    fn put_part(&mut self, data: PutPayload) -> UploadPart {
        // Never held here: `abort` and `complete` run only once no part is being added.
        match self.0.try_lock() {
            Ok(mut upload) => upload.put_part(data),
            Err(_) => Box::pin(async { Err(object_store::Error::NotImplemented) }),
        }
    }

    async fn complete(&mut self) -> object_store::Result<PutResult> {
        self.0.lock().await.complete().await
    }

    async fn abort(&mut self) -> object_store::Result<()> {
        self.0.lock().await.abort().await
    }
}

const SWEEP_LOCK: i64 = 0x004d_4544_4941_5357;

/// The store and the client that downloads into it.
#[derive(Clone)]
pub struct Media {
    pub store: Arc<dyn ObjectStore>,
    client: reqwest::Client,
    allow_private: bool,
    /// The largest video stored: `MAX_BYTES`, or `MEDIA_MAX_BYTES` when that is lower. Only a
    /// video's cap: an image or a PDF keeps `MAX_STILL_BYTES`.
    pub max_bytes: usize,
    /// The store's directory when it is local disk (`MEDIA_DIR`), for `sweep_temp`.
    pub dir: Option<std::path::PathBuf>,
    /// `INGEST_DEADLINE`; tests shorten it.
    pub deadline: Duration,
    /// `INGEST_CEILING`; tests shorten it.
    pub ceiling: Duration,
    /// `STALL`; tests shorten it.
    pub stall: Duration,
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
    /// Google Drive's "too many users have viewed or downloaded this file recently" page.
    DriveQuota,
    /// Google Drive kept answering with its "can't scan this file for viruses" page.
    DriveConfirm,
    Http(u16),
    Unreachable,
    /// Over the cap it was downloaded with (`Media::max_bytes`, or a cover's limit).
    TooLarge,
    /// Not a video, and over `MAX_STILL_BYTES`.
    TooLargeStill,
    /// Not a JPEG, PNG, WebP, GIF, MP4, MOV or PDF.
    Unsupported,
    Storage,
    /// The download ran past its deadline (`INGEST_DEADLINE` plus its size at `MIN_SPEED`), or
    /// the post's downloads past `INGEST_CEILING`, before this one was stored.
    TimedOut,
    /// The server sent nothing for `STALL`.
    Stalled,
}

/// A stored file.
#[derive(Clone, Debug)]
pub struct Stored {
    pub key: String,
    pub content_type: String,
    pub size: i64,
}

/// A file's bytes for a publisher: held in memory (an image, a cover, a PDF, each at most
/// `MAX_STILL_BYTES`), or left in the store and read a range at a time (a video, and any file a
/// platform fetches by its link), so a video is never held whole however large it is.
#[derive(Clone)]
pub enum Blob {
    Held(bytes::Bytes),
    Stored {
        store: Arc<dyn ObjectStore>,
        key: String,
        size: u64,
    },
}

impl Default for Blob {
    fn default() -> Self {
        Blob::Held(bytes::Bytes::new())
    }
}

impl Blob {
    pub fn len(&self) -> u64 {
        match self {
            Blob::Held(bytes) => bytes.len() as u64,
            Blob::Stored { size, .. } => *size,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The bytes held in memory. A stored file has none (`range` and `body` read it), so asking
    /// for them is a failure, never an empty upload.
    pub fn held(&self) -> Result<bytes::Bytes, PostError> {
        match self {
            Blob::Held(bytes) => Ok(bytes.clone()),
            Blob::Stored { key, .. } => {
                tracing::error!(key, "a stored file was asked for whole");
                Err(PostError::Failed(
                    "The file could not be read. Try again.".into(),
                ))
            }
        }
    }

    fn unread(e: object_store::Error) -> PostError {
        tracing::warn!(error = %e, "stored media read failed");
        PostError::Failed("The stored file could not be read. Try again.".into())
    }

    /// Bytes `range` of the file, cut off at its end: one ranged read of the store.
    pub async fn range(&self, range: std::ops::Range<u64>) -> Result<bytes::Bytes, PostError> {
        let range = range.start.min(self.len())..range.end.min(self.len());
        match self {
            Blob::Held(bytes) => Ok(bytes.slice(range.start as usize..range.end as usize)),
            Blob::Stored { .. } if range.is_empty() => Ok(bytes::Bytes::new()),
            Blob::Stored { store, key, .. } => store
                .get_range(&Path::from(key.as_str()), range)
                .await
                .map_err(Self::unread),
        }
    }

    /// The whole file as a request body, streamed from the store as it is sent. Its length is
    /// `len`, which the caller sends as `Content-Length`.
    pub async fn body(&self) -> Result<reqwest::Body, PostError> {
        match self {
            Blob::Held(bytes) => Ok(bytes.clone().into()),
            Blob::Stored { store, key, .. } => {
                let got = store.get(&Path::from(key.as_str())).await;
                Ok(reqwest::Body::wrap_stream(
                    got.map_err(Self::unread)?.into_stream(),
                ))
            }
        }
    }
}

impl Media {
    /// The stored file `file` as a `Blob` that reads it from the store.
    pub fn blob(&self, file: &Stored) -> Blob {
        Blob::Stored {
            store: self.store.clone(),
            key: file.key.clone(),
            size: file.size.max(0) as u64,
        }
    }

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
            // No total timeout: a large file takes as long as its size allows (`download`).
            .read_timeout(STALL)
            .redirect(redirects)
            .dns_resolver(Arc::new(PublicOnly))
            .user_agent("SocialReins")
            .build()
            .expect("the media http client builds");
        Self {
            store,
            client,
            allow_private,
            max_bytes: MAX_BYTES,
            dir: None,
            deadline: INGEST_DEADLINE,
            ceiling: INGEST_CEILING,
            stall: STALL,
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
        let first = Instant::now() + self.probe_deadline;
        let mut res = self.open(url, true, first).await.ok()?;
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

    /// `ingest` of every URL, `PARALLEL` at a time, each within its own deadline (`download`) and
    /// all within `ceiling`; a URL not stored by then is `TimedOut`, its upload aborted.
    pub async fn ingest_all(
        &self,
        db: &DatabaseConnection,
        (post, group): (Uuid, Option<Uuid>),
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
        let until = Instant::now() + self.ceiling;
        let mut found = HashMap::new();
        let mut running = tokio::task::JoinSet::new();
        loop {
            while running.len() < PARALLEL {
                let Some(url) = left.pop() else { break };
                let (media, db) = (self.clone(), db.clone());
                running.spawn(async move {
                    let got = media.ingest(&db, (post, group), &url, cap, until).await;
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
    /// at most `cap` bytes (and never over `max_bytes`, or `MAX_STILL_BYTES` unless it is a
    /// video), by `until`. A row whose file is gone is
    /// a miss: it is deleted and the URL downloaded again. A video another post of `group` (the
    /// post's `group_id`) already stored from the same URL is copied in the store instead
    /// (`copied`), so a group downloads it once; each post still owns, and later deletes, its own
    /// file. Posts of a group take turns per URL, only until a file is known not to be a video.
    pub async fn ingest(
        &self,
        db: &DatabaseConnection,
        (post, group): (Uuid, Option<Uuid>),
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
        let mut turn = None;
        let mut copy = None;
        if let Some(group) = group {
            let wait = tokio::time::timeout_at(until, turn_of(group, url).lock_owned());
            turn = Some(wait.await.map_err(|_| Fail::TimedOut)?);
            let video_cap = cap.min(self.max_bytes);
            copy = self.copied(db, post, url, video_cap, until).await;
        }
        let (key, content_type, size) = match copy {
            Some(copy) => copy,
            None => {
                let got = self.download(post, url, cap, until, &mut turn).await;
                got.map(|(key, content_type, size)| (key, content_type.to_owned(), size))?
            }
        };
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
        drop(turn);
        kept
    }

    /// A copy for `post`, made inside the store (no bytes pass through brain), of the video at
    /// most `cap` bytes that another post of its group stored from `url`: its key, type and size.
    /// `None` when there is none or the copy fails (the other post settled and its file went),
    /// and the caller downloads.
    async fn copied(
        &self,
        db: &DatabaseConnection,
        post: Uuid,
        url: &str,
        cap: usize,
        until: Instant,
    ) -> Option<(String, String, usize)> {
        let stmt = sql(
            "SELECT m.storage_key, m.content_type, m.size FROM posts me \
             JOIN posts p ON p.team_id = me.team_id AND p.group_id = me.group_id AND p.id <> me.id \
             JOIN post_media m ON m.post_id = p.id \
             WHERE me.id = $1 AND m.source_url = $2 AND m.content_type LIKE 'video/%' \
             AND m.size <= $3 ORDER BY m.created_at DESC LIMIT 1",
            vec![post.into(), url.into(), (cap as i64).into()],
        );
        let from = stored(&db.query_one_raw(stmt).await.ok()??).ok()?;
        let key = format!("posts/{post}/{}", Uuid::now_v7());
        let (source, to) = (Path::from(from.key.as_str()), Path::from(key.as_str()));
        let copy = self.store.copy(&source, &to);
        match tokio::time::timeout_at(until, copy).await {
            Ok(Ok(())) => Some((key, from.content_type, from.size.max(0) as usize)),
            failed => {
                if let Ok(Err(e)) = failed {
                    tracing::info!(error = %e, post = %post, "group media copy failed");
                }
                self.delete(&key).await;
                None
            }
        }
    }

    /// Tests only: `url` on the mock that stands in for its host (`probe_rewrite`).
    fn rewritten(&self, url: &str) -> String {
        let to = self.probe_rewrite.as_ref().filter(|_| self.allow_private);
        to.and_then(|(from, to)| Some(format!("{to}{}", url.strip_prefix(from.as_str())?)))
            .unwrap_or_else(|| url.to_owned())
    }

    /// The answer to a GET of `url` (a Drive share link as its direct download; only its first
    /// bytes when `probing`), started by `first`. Drive answers a large file with a page asking
    /// to confirm the download: that page's form is followed once, to a Drive host only, and
    /// whatever page Drive sends in place of the file is told apart here (`NotShared`,
    /// `DriveQuota`, `DriveConfirm`). Every URL asked is public (`public_url`, `PublicOnly`).
    async fn open(
        &self,
        url: &str,
        probing: bool,
        first: Instant,
    ) -> Result<reqwest::Response, Fail> {
        let drive = drive_url(url);
        let target = |url: &str| {
            let url = if drive.is_some() || probing {
                self.rewritten(url)
            } else {
                url.to_owned()
            };
            let ok = |u: &Url| public_url(u, self.allow_private);
            Url::parse(&url).ok().filter(ok).ok_or(Fail::Blocked)
        };
        let get = |from: Url| async move {
            let mut req = self.client.get(from);
            if probing {
                req = req.header(reqwest::header::RANGE, format!("bytes=0-{}", PROBE_LEN - 1));
            }
            let sent = tokio::time::timeout_at(first, req.send()).await;
            sent.map_err(|_| Fail::TimedOut)?.map_err(|e| {
                let fail = if e.is_redirect() || blocked(&e) {
                    Fail::Blocked
                } else {
                    Fail::Unreachable
                };
                tracing::warn!(error = %e.without_url(), "media download failed");
                fail
            })
        };
        let html = |res: &reqwest::Response| {
            let ty = res
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok());
            ty.is_some_and(|v| v.to_ascii_lowercase().starts_with("text/html"))
        };
        let mut res = get(target(drive.as_deref().unwrap_or(url))?).await?;
        if drive.is_some() && html(&res) {
            let status = res.status();
            let mut page = Vec::new();
            while page.len() < DRIVE_PAGE_BYTES {
                let chunk = tokio::time::timeout_at(first, res.chunk()).await;
                match chunk
                    .map_err(|_| Fail::TimedOut)?
                    .map_err(|_| Fail::Unreachable)?
                {
                    Some(chunk) => page.extend_from_slice(&chunk),
                    None => break,
                }
            }
            page.truncate(DRIVE_PAGE_BYTES);
            match drive_page(&String::from_utf8_lossy(&page)) {
                Ok(next) => res = get(target(next.as_str())?).await?,
                Err(Fail::NotShared) if status != StatusCode::OK => {
                    return Err(Fail::Http(status.as_u16()))
                }
                Err(fail) => return Err(fail),
            }
            if html(&res) {
                return Err(Fail::DriveConfirm);
            }
        }
        Ok(res)
    }

    /// Streams `url` (a Drive share link as its direct download) into a new object, part by part
    /// (`PART_BYTES`), and returns its key, type and size. The answer must start within
    /// `deadline`, the file may take `deadline` plus its size at `MIN_SPEED`, never past `until`,
    /// and a server silent for `stall` fails at once. A file over `cap` is refused by its
    /// `Content-Length` before a byte is read, else the moment the bytes pass it; whatever stops
    /// a download, its partial upload is aborted, so nothing stays in the store.
    async fn download(
        &self,
        post: Uuid,
        url: &str,
        cap: usize,
        until: Instant,
        turn: &mut Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<(String, &'static str, usize), Fail> {
        let started = Instant::now();
        let first = until.min(started + self.deadline);
        let mut res = self.open(url, false, first).await.inspect_err(
            |fail| tracing::warn!(?fail, post = %post, "media download did not start"),
        )?;
        if res.status() != StatusCode::OK {
            return Err(Fail::Http(res.status().as_u16()));
        }
        // `max_bytes` caps a video only, and nothing says yet whether this is one.
        let video_cap = cap.min(self.max_bytes);
        let any_cap = video_cap.max(cap.min(MAX_STILL_BYTES));
        if res.content_length().is_some_and(|n| n > any_cap as u64) {
            return Err(if any_cap == video_cap {
                Fail::TooLarge
            } else {
                Fail::TooLargeStill
            });
        }
        let expected = res.content_length().unwrap_or(cap as u64);
        let until = until.min(started + self.deadline + Duration::from_secs(expected / MIN_SPEED));
        let stall = self.stall;
        let read = |e: reqwest::Error| {
            tracing::warn!(error = %e.without_url(), post = %post, "media download failed");
            Fail::Unreachable
        };
        // Only the first bytes are kept to tell the type; the rest goes straight to storage.
        let mut head = Vec::new();
        while head.len() < SNIFF_LEN {
            let chunk = tokio::time::timeout_at(until, tokio::time::timeout(stall, res.chunk()));
            let chunk = chunk.await.map_err(|_| Fail::TimedOut)?;
            match chunk.map_err(|_| Fail::Stalled)?.map_err(read)? {
                Some(chunk) => head.extend_from_slice(&chunk),
                None => break,
            }
        }
        // A page Drive sent in place of a file never gets here (`open`).
        let content_type = sniff(&head).ok_or(Fail::Unsupported)?;
        // Only a video is shared in a group, so nothing else keeps the group's other posts waiting.
        if !content_type.starts_with("video/") {
            turn.take();
        }
        // Only a video may pass `MAX_STILL_BYTES`: anything else is read whole later.
        let (cap, over) = if content_type.starts_with("video/") {
            (video_cap, Fail::TooLarge)
        } else if cap <= MAX_STILL_BYTES {
            (cap, Fail::TooLarge)
        } else {
            (MAX_STILL_BYTES, Fail::TooLargeStill)
        };
        let mut size = head.len();
        if size > cap || res.content_length().is_some_and(|n| n > cap as u64) {
            return Err(over);
        }
        let key = format!("posts/{post}/{}", Uuid::now_v7());
        let upload = self
            .store
            .put_multipart(&Path::from(key.as_str()))
            .await
            .map_err(storage)?;
        let mut upload = Upload(Arc::new(tokio::sync::Mutex::new(upload)));
        let mut file = WriteMultipart::new_with_chunk_size(Box::new(upload.clone()), PART_BYTES);
        file.write(&head);
        let streamed = async {
            loop {
                let chunk = tokio::time::timeout(stall, res.chunk()).await;
                let Some(chunk) = chunk.map_err(|_| Fail::Stalled)?.map_err(read)? else {
                    return Ok(());
                };
                size += chunk.len();
                if size > cap {
                    return Err(over);
                }
                // Storage slower than the download holds the download back, not the bytes.
                file.wait_for_capacity(PARTS_IN_FLIGHT)
                    .await
                    .map_err(storage)?;
                file.write(&chunk);
            }
        };
        // Past the deadline the upload is aborted, so no partial file stays behind.
        let streamed = tokio::time::timeout_at(until, streamed)
            .await
            .unwrap_or(Err(Fail::TimedOut));
        match streamed {
            Ok(()) => {
                if let Err(e) = file.finish().await {
                    // A second abort after `finish` made its own is refused, which is fine.
                    let _ = upload.abort().await;
                    return Err(storage(e));
                }
            }
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
    let mut gone = app.media.release(&app.db, None, SWEEP_DAYS).await?;
    if let Some(dir) = app.media.dir.clone() {
        let swept = tokio::task::spawn_blocking(move || sweep_temp(&dir, TEMP_AGE));
        gone += swept.await.unwrap_or(0);
    }
    lock.commit().await?;
    Ok(gone)
}

/// A temp file on local disk this old belongs to no download still running: several times
/// `INGEST_CEILING`.
const TEMP_AGE: Duration = Duration::from_secs(3600);

/// Deletes the temp files under `dir` older than `older`, and returns how many went. The local
/// store writes a file as `<key>#<n>` and renames it when done, so a process killed
/// mid-download leaves one (up to `MAX_BYTES`) that no listing shows and `release` never sees.
/// In a bucket the same leftovers are parts of a multipart upload, which its lifecycle rule
/// aborts (README).
fn sweep_temp(dir: &std::path::Path, older: Duration) -> u64 {
    let mut gone = 0;
    let Ok(posts) = std::fs::read_dir(dir.join("posts")) else {
        return 0;
    };
    for post in posts.flatten() {
        let Ok(files) = std::fs::read_dir(post.path()) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name();
            let digits = |n: &str| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
            let temp = name
                .to_string_lossy()
                .rsplit_once('#')
                .is_some_and(|n| digits(n.1));
            let age = file.metadata().and_then(|m| m.modified()).ok();
            let old = age
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|a| a >= older);
            if temp && old && std::fs::remove_file(file.path()).is_ok() {
                gone += 1;
            }
        }
    }
    gone
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

/// The most of a Drive page that is read to tell what it is.
const DRIVE_PAGE_BYTES: usize = 256 * 1024;
/// Hosts the form of Drive's confirmation page may send the download to.
const DRIVE_HOSTS: [&str; 3] = [
    "drive.usercontent.google.com",
    "drive.google.com",
    "docs.google.com",
];

/// The value of attribute `name` in an HTML tag, quoted either way.
fn attr(tag: &str, name: &str) -> Option<String> {
    for quote in ['"', '\''] {
        if let Some(at) = tag.find(&format!(" {name}={quote}")) {
            let value = &tag[at + name.len() + 3..];
            return Some(value[..value.find(quote)?].replace("&amp;", "&"));
        }
    }
    None
}

/// What a page Drive sent in place of a file says: the download its "can't scan this file for
/// viruses" form asks for (the form's action with its hidden fields: `id`, `export`, `confirm`,
/// `uuid`, `at`), else why there is none.
fn drive_page(page: &str) -> Result<Url, Fail> {
    let lower = page.to_ascii_lowercase();
    if lower.contains("too many users have viewed or downloaded")
        || lower.contains("quota exceeded")
    {
        return Err(Fail::DriveQuota);
    }
    let Some(form) = page
        .split("<form")
        .skip(1)
        .find(|f| f.contains("download-form"))
    else {
        return Err(if lower.contains("virus") {
            Fail::DriveConfirm
        } else {
            Fail::NotShared
        });
    };
    let form = form.split("</form").next().unwrap_or(form);
    let mut tags = form.split('<').map(|t| t.split('>').next().unwrap_or(t));
    let action = tags.next().and_then(|tag| attr(tag, "action"));
    let mut to = action
        .and_then(|a| Url::parse(&a).ok())
        .ok_or(Fail::DriveConfirm)?;
    let host = to.host_str().unwrap_or_default();
    if to.scheme() != "https" || !DRIVE_HOSTS.contains(&host) {
        return Err(Fail::DriveConfirm);
    }
    let fields: Vec<(String, String)> = tags
        .filter(|tag| tag.starts_with("input"))
        .filter_map(|tag| Some((attr(tag, "name")?, attr(tag, "value").unwrap_or_default())))
        .collect();
    if !fields
        .iter()
        .any(|(name, value)| name == "id" && !value.is_empty())
    {
        return Err(Fail::DriveConfirm);
    }
    to.query_pairs_mut().clear().extend_pairs(&fields);
    Ok(to)
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
    fn old_temp_files_on_disk_are_swept() {
        let dir = std::env::temp_dir().join(format!("media-temp-{}", Uuid::now_v7()));
        let post = dir.join("posts").join(Uuid::now_v7().to_string());
        std::fs::create_dir_all(&post).unwrap();
        let long_ago = std::time::SystemTime::now() - 2 * TEMP_AGE;
        // A download cut off long ago, one still running, and a stored file as old as the first.
        for (name, old) in [("a#1", true), ("b#12", false), ("c", true), ("d#x", true)] {
            let file = std::fs::File::create(post.join(name)).unwrap();
            if old {
                file.set_modified(long_ago).unwrap();
            }
        }
        assert_eq!(sweep_temp(&dir, TEMP_AGE), 1);
        let mut left: Vec<_> = std::fs::read_dir(&post)
            .unwrap()
            .map(|f| f.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["b#12", "c", "d#x"]);
        assert_eq!(sweep_temp(&dir.join("none"), TEMP_AGE), 0);
        std::fs::remove_dir_all(&dir).unwrap();
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
                .download(post, &format!("{base}{path}"), MAX_BYTES, soon(), &mut None)
                .await
                .unwrap();
            assert_eq!((ty, size), ("image/png", 100_000));
            assert!(key.starts_with(&format!("posts/{post}/")));
            assert_eq!(media.read(&key).await.unwrap(), png(100_000));
        }
        let got = media
            .download(post, &format!("{base}/ok.png"), 99_999, soon(), &mut None)
            .await;
        assert_eq!(got.unwrap_err(), Fail::TooLarge);
        let got = media
            .download(post, &format!("{base}/page"), MAX_BYTES, soon(), &mut None)
            .await;
        assert_eq!(got.unwrap_err(), Fail::Unsupported);
    }

    /// Stands in for `drive.usercontent.google.com/download`: file `big` comes only once the
    /// confirmation form's `uuid` and `at` are sent back, `stuck` never does, `evil`'s form
    /// points off Drive, `quota` is over its download quota and `private` asks to sign in.
    async fn drive() -> String {
        use axum::{extract::Query, response::IntoResponse, routing::get, Router};
        const DRIVE: &str = "https://drive.usercontent.google.com/download";
        let page = |id: &str, action: &str| {
            let form = format!(
                "<!DOCTYPE html><html><head><title>Google Drive - Virus scan warning</title>\
                 </head><body><p>Google Drive can't scan this file for viruses.</p>\
                 <form id=\"download-form\" action=\"{action}\" method=\"get\">\
                 <input type=\"submit\" id=\"uc-download-link\" value=\"Download anyway\"/>\
                 <input type=\"hidden\" name=\"id\" value=\"{id}\">\
                 <input type=\"hidden\" name=\"export\" value=\"download\">\
                 <input type=\"hidden\" name=\"confirm\" value=\"t\">\
                 <input type=\"hidden\" name=\"uuid\" value=\"u-1&amp;2\">\
                 <input type=\"hidden\" name=\"at\" value=\"tok\"></form></body></html>"
            );
            ([("content-type", "text/html; charset=utf-8")], form).into_response()
        };
        let html = |body: &'static str| ([("content-type", "text/html")], body).into_response();
        let serve = move |Query(q): Query<HashMap<String, String>>| async move {
            let confirmed = q.get("uuid").is_some_and(|u| u == "u-1&2")
                && q.get("at").is_some_and(|a| a == "tok")
                && q.get("export").is_some_and(|e| e == "download");
            match q.get("id").map(String::as_str) {
                Some("big") if confirmed => {
                    let mut video = MP4_HEAD.to_vec();
                    video.resize(300_000, 7);
                    ([("content-type", "video/mp4")], video).into_response()
                }
                Some(id @ ("big" | "stuck")) => page(id, DRIVE),
                Some("evil") => page("evil", "https://evil.test/download"),
                Some("quota") => html(
                    "<html><title>Google Drive - Quota exceeded</title><body>Sorry, you can't \
                     view or download this file at this time. Too many users have viewed or \
                     downloaded this file recently.</body></html>",
                ),
                _ => html("<html><body>Sign in to continue to Google Drive</body></html>"),
            }
        };
        let app = Router::new().route("/download", get(serve));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        base
    }

    #[tokio::test]
    async fn a_large_drive_file_is_fetched_through_its_confirmation_page() {
        let mut media = Media::new(Arc::new(object_store::memory::InMemory::new()), true);
        media.probe_rewrite = Some(("https://drive.usercontent.google.com".into(), drive().await));
        let link = |id: &str| format!("https://drive.google.com/file/d/{id}/view?usp=sharing");
        let post = Uuid::now_v7();

        let (key, ty, size) = media
            .download(post, &link("big"), MAX_BYTES, soon(), &mut None)
            .await
            .unwrap();
        assert_eq!((ty, size), ("video/mp4", 300_000));
        assert_eq!(media.read(&key).await.unwrap().len(), 300_000);
        // The save-time probe gets past the page the same way.
        let probed = media.probe(&link("big")).await.unwrap();
        assert_eq!(probed.content_type, "video/mp4");

        for (id, why) in [
            ("quota", Fail::DriveQuota),
            ("stuck", Fail::DriveConfirm),
            ("evil", Fail::DriveConfirm),
            ("private", Fail::NotShared),
        ] {
            let got = media
                .download(post, &link(id), MAX_BYTES, soon(), &mut None)
                .await;
            assert_eq!(got.unwrap_err(), why, "{id}");
            assert_eq!(media.probe(&link(id)).await, None, "{id}");
        }
    }

    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    use futures_util::{stream, StreamExt};
    use object_store::{
        GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, PutMultipartOptions,
        PutOptions, PutPayload, PutResult, UploadPart,
    };

    const MP4_HEAD: &[u8] = b"\0\0\0\x18ftypmp42\0\0\0\0";

    /// `len` bytes of video made as they are sent, 64 KiB at a time, counted in `sent`.
    fn video_stream(len: usize, sent: Arc<AtomicUsize>) -> axum::body::Body {
        let chunks = stream::unfold(0usize, move |at| {
            let sent = sent.clone();
            async move {
                let n = (len - at).min(65_536);
                let mut chunk = vec![7u8; n];
                if at == 0 {
                    chunk[..MP4_HEAD.len()].copy_from_slice(MP4_HEAD);
                }
                sent.fetch_add(n, SeqCst);
                (n > 0).then(|| (Ok::<_, std::io::Error>(chunk), at + n))
            }
        });
        axum::body::Body::from_stream(chunks)
    }

    /// Serves `/sized.mp4` (`len` bytes with a `Content-Length`), `/unsized.mp4` (the same,
    /// length unsaid), `/stall.mp4` (a first chunk, then nothing) and `/slow.mp4` (a few bytes
    /// every 20 ms, without end). `sent` counts the bytes the server made.
    async fn video_server(len: usize, sent: Arc<AtomicUsize>) -> String {
        use axum::{body::Body, routing::get, Router};
        let (a, b) = (sent.clone(), sent);
        let app = Router::new()
            .route(
                "/sized.mp4",
                get(move || {
                    let body = video_stream(len, a.clone());
                    async move { ([("content-length", len.to_string())], body) }
                }),
            )
            .route(
                "/unsized.mp4",
                get(move || {
                    let body = video_stream(len, b.clone());
                    async move { body }
                }),
            )
            .route(
                "/stall.mp4",
                get(|| async {
                    let first = stream::once(async { Ok::<_, std::io::Error>(vec![7u8; 100_000]) });
                    let mut head = MP4_HEAD.to_vec();
                    head.resize(1000, 7);
                    let head = stream::once(async move { Ok(head) });
                    Body::from_stream(head.chain(first).chain(stream::pending()))
                }),
            )
            .route(
                "/slow.mp4",
                get(|| async {
                    Body::from_stream(stream::unfold((), |()| async {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Some((Ok::<_, std::io::Error>(MP4_HEAD.to_vec()), ()))
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        base
    }

    /// A store that counts: the bytes whose part is written (`done`), the most bytes that were
    /// sent by the server but not yet written when a part arrived (`peak`), the largest part,
    /// and the uploads aborted. Parts are written slowly, so the download has to wait for them.
    #[derive(Debug, Default)]
    struct Counting {
        inner: object_store::memory::InMemory,
        sent: Arc<AtomicUsize>,
        done: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        largest: Arc<AtomicUsize>,
        aborted: Arc<AtomicUsize>,
        /// Not zero: every part fails to store.
        failing: Arc<AtomicUsize>,
    }

    impl std::fmt::Display for Counting {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "Counting")
        }
    }

    #[derive(Debug)]
    struct CountingUpload {
        inner: Box<dyn MultipartUpload>,
        sent: Arc<AtomicUsize>,
        done: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        largest: Arc<AtomicUsize>,
        aborted: Arc<AtomicUsize>,
        failing: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl MultipartUpload for CountingUpload {
        fn put_part(&mut self, data: PutPayload) -> UploadPart {
            let len = data.content_length();
            self.largest.fetch_max(len, SeqCst);
            let waiting = self
                .sent
                .load(SeqCst)
                .saturating_sub(self.done.load(SeqCst));
            self.peak.fetch_max(waiting, SeqCst);
            let (part, done) = (self.inner.put_part(data), self.done.clone());
            let failing = self.failing.load(SeqCst) != 0;
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                if failing {
                    return Err(object_store::Error::NotImplemented);
                }
                part.await?;
                done.fetch_add(len, SeqCst);
                Ok(())
            })
        }

        async fn complete(&mut self) -> object_store::Result<PutResult> {
            self.inner.complete().await
        }

        async fn abort(&mut self) -> object_store::Result<()> {
            self.aborted.fetch_add(1, SeqCst);
            self.inner.abort().await
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for Counting {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &Path,
            opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            Ok(Box::new(CountingUpload {
                inner: self.inner.put_multipart_opts(location, opts).await?,
                sent: self.sent.clone(),
                done: self.done.clone(),
                peak: self.peak.clone(),
                largest: self.largest.clone(),
                aborted: self.aborted.clone(),
                failing: self.failing.clone(),
            }))
        }

        async fn get_opts(
            &self,
            location: &Path,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            self.inner.get_opts(location, options).await
        }

        async fn delete(&self, location: &Path) -> object_store::Result<()> {
            self.inner.delete(location).await
        }

        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> stream::BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy(&self, from: &Path, to: &Path) -> object_store::Result<()> {
            self.inner.copy(from, to).await
        }

        async fn copy_if_not_exists(&self, from: &Path, to: &Path) -> object_store::Result<()> {
            self.inner.copy_if_not_exists(from, to).await
        }
    }

    /// The most a download may hold: its parts, plus what sits in socket and HTTP buffers.
    const HELD: usize = PART_BYTES * (PARTS_IN_FLIGHT + 1) + 8 * 1024 * 1024;

    #[tokio::test]
    async fn a_large_video_streams_into_the_store_in_bounded_parts() {
        // Three times what a download may hold, so holding it whole would show.
        let len = 3 * HELD + 12_345;
        for path in ["/sized.mp4", "/unsized.mp4"] {
            let store = Arc::new(Counting::default());
            let base = video_server(len, store.sent.clone()).await;
            let media = Media::new(store.clone(), true);
            let (key, ty, size) = media
                .download(
                    Uuid::now_v7(),
                    &format!("{base}{path}"),
                    MAX_BYTES,
                    soon(),
                    &mut None,
                )
                .await
                .unwrap();
            assert_eq!((ty, size), ("video/mp4", len), "{path}");
            let meta = store.inner.head(&Path::from(key)).await.unwrap();
            assert_eq!(meta.size, len as u64);
            assert_eq!(store.done.load(SeqCst), len);
            assert_eq!(store.largest.load(SeqCst), PART_BYTES, "{path}");
            let peak = store.peak.load(SeqCst);
            assert!(peak <= HELD, "{path}: {peak} bytes waited for the store");
        }
    }

    #[tokio::test]
    async fn a_file_over_the_cap_is_refused_before_or_while_it_downloads() {
        let len = 40_000_000;
        let store = Arc::new(Counting::default());
        let base = video_server(len, store.sent.clone()).await;
        let media = Media::new(store.clone(), true);
        let post = Uuid::now_v7();
        // Its size said up front: refused on the headers, nothing stored or even started.
        let got = media
            .download(
                post,
                &format!("{base}/sized.mp4"),
                len - 1,
                soon(),
                &mut None,
            )
            .await;
        assert_eq!(got.unwrap_err(), Fail::TooLarge);
        assert_eq!(store.largest.load(SeqCst), 0, "no part was written");
        assert!(store.sent.load(SeqCst) < len, "the file was not downloaded");

        // Its size unsaid: stopped the moment the bytes pass the cap, on disk as in a bucket,
        // and the partial file is gone.
        let dir = std::env::temp_dir().join(format!("media-cap-{}", Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let disk = object_store::local::LocalFileSystem::new_with_prefix(&dir).unwrap();
        let on_disk = Media::new(Arc::new(disk), true);
        let cap = 12_000_000;
        for media in [&media, &on_disk] {
            store.sent.store(0, SeqCst);
            let got = media
                .download(post, &format!("{base}/unsized.mp4"), cap, soon(), &mut None)
                .await;
            assert_eq!(got.unwrap_err(), Fail::TooLarge);
            assert!(store.sent.load(SeqCst) < len, "the download stopped early");
        }
        assert_eq!(store.aborted.load(SeqCst), 1, "the upload was aborted");
        let stored = store.inner.list_with_delimiter(None).await.unwrap();
        assert!(stored.objects.is_empty() && stored.common_prefixes.is_empty());
        let left: Vec<_> = std::fs::read_dir(dir.join("posts").join(post.to_string()))
            .map(|d| d.flatten().map(|f| f.path()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "{left:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_download_the_store_does_not_take_aborts_its_upload() {
        // One part, sent only as the upload is finished, and several, the first failing midway:
        // either way the upload is aborted, so a bucket keeps no parts of it.
        for (len, path) in [(100_000, "/sized.mp4"), (3 * PART_BYTES, "/unsized.mp4")] {
            let store = Arc::new(Counting::default());
            store.failing.store(1, SeqCst);
            let base = video_server(len, store.sent.clone()).await;
            let media = Media::new(store.clone(), true);
            let got = media
                .download(
                    Uuid::now_v7(),
                    &format!("{base}{path}"),
                    MAX_BYTES,
                    soon(),
                    &mut None,
                )
                .await;
            assert_eq!(got.unwrap_err(), Fail::Storage, "{len}");
            assert!(
                store.aborted.load(SeqCst) >= 1,
                "{len}: the upload was aborted"
            );
            let stored = store.inner.list_with_delimiter(None).await.unwrap();
            assert!(stored.objects.is_empty() && stored.common_prefixes.is_empty());
        }
    }

    #[tokio::test]
    async fn only_a_video_may_be_larger_than_an_image_or_a_pdf() {
        use axum::{routing::get, Router};
        // A PNG one byte over the cap of files that are not videos, sized and unsized.
        let big = || {
            let mut sent = 0;
            axum::body::Body::from_stream(stream::poll_fn(move |_| {
                let chunk = if sent == 0 {
                    png(65_536)
                } else {
                    vec![7; (1 << 20).min(MAX_STILL_BYTES + 1 - sent)]
                };
                sent += chunk.len();
                std::task::Poll::Ready(
                    (!chunk.is_empty()).then_some(Ok::<_, std::io::Error>(chunk)),
                )
            }))
        };
        let len = (MAX_STILL_BYTES + 1).to_string();
        let app = Router::new()
            .route(
                "/sized.png",
                get(move || async move { ([("content-length", len)], big()) }),
            )
            .route("/unsized.png", get(move || async move { big() }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let store = Arc::new(Counting::default());
        let mut media = Media::new(store.clone(), true);
        for path in ["/sized.png", "/unsized.png"] {
            let got = media
                .download(
                    Uuid::now_v7(),
                    &format!("{base}{path}"),
                    MAX_BYTES,
                    soon(),
                    &mut None,
                )
                .await;
            assert_eq!(got.unwrap_err(), Fail::TooLargeStill, "{path}");
        }
        let stored = store.inner.list_with_delimiter(None).await.unwrap();
        assert!(stored.objects.is_empty() && stored.common_prefixes.is_empty());

        // `MEDIA_MAX_BYTES` lowers the cap of a video (`max_bytes`), sized or not, and never
        // an image's: the 100 KB PNG is over it and still stored.
        media.max_bytes = 50_000;
        let base = video_server(50_001, store.sent.clone()).await;
        for path in ["/sized.mp4", "/unsized.mp4"] {
            let got = media
                .download(
                    Uuid::now_v7(),
                    &format!("{base}{path}"),
                    MAX_BYTES,
                    soon(),
                    &mut None,
                )
                .await;
            assert_eq!(got.unwrap_err(), Fail::TooLarge, "{path}");
        }
        let got = media
            .download(
                Uuid::now_v7(),
                &format!("{}/ok.png", server().await),
                MAX_BYTES,
                soon(),
                &mut None,
            )
            .await;
        assert_eq!(got.unwrap().2, 100_000);
    }

    #[tokio::test]
    async fn a_stalled_or_too_slow_download_fails_fast_and_stores_nothing() {
        let store = Arc::new(Counting::default());
        let base = video_server(0, store.sent.clone()).await;
        let mut media = Media::new(store.clone(), true);
        media.stall = Duration::from_millis(300);
        let started = Instant::now();
        let got = media
            .download(
                Uuid::now_v7(),
                &format!("{base}/stall.mp4"),
                MAX_BYTES,
                soon(),
                &mut None,
            )
            .await;
        assert_eq!(got.unwrap_err(), Fail::Stalled);
        assert!(started.elapsed() < Duration::from_secs(3), "failed fast");

        // Bytes keep coming, but slower than `MIN_SPEED`: the deadline its size allows ends it.
        // Size unsaid, so that is the cap's: 0.3 s and 2 MB at 2 MB a second.
        media.deadline = Duration::from_millis(300);
        let started = Instant::now();
        let got = media
            .download(
                Uuid::now_v7(),
                &format!("{base}/slow.mp4"),
                2_000_000,
                soon(),
                &mut None,
            )
            .await;
        assert_eq!(got.unwrap_err(), Fail::TimedOut);
        let took = started.elapsed();
        assert!(
            took >= Duration::from_secs(1) && took < Duration::from_secs(4),
            "{took:?}"
        );
        // A larger cap allows more time, and the post's ceiling ends it whatever the size.
        media.ceiling = Duration::from_millis(200);
        let until = Instant::now() + media.ceiling;
        let started = Instant::now();
        let got = media
            .download(
                Uuid::now_v7(),
                &format!("{base}/slow.mp4"),
                MAX_BYTES,
                until,
                &mut None,
            )
            .await;
        assert_eq!(got.unwrap_err(), Fail::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));

        assert_eq!(store.aborted.load(SeqCst), 3, "each upload was aborted");
        let stored = store.inner.list_with_delimiter(None).await.unwrap();
        assert!(stored.objects.is_empty() && stored.common_prefixes.is_empty());
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
                    .download(post, &url, MAX_BYTES, soon(), &mut None)
                    .await
                    .unwrap_err(),
                Fail::Blocked,
                "{url}"
            );
        }
    }
}
