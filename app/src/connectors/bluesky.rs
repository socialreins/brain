//! Bluesky: no OAuth. A handle and an app password are checked by resolving the handle to a
//! DID, the DID to its PDS, and opening a session on that PDS. The app password only ever goes
//! to the resolved PDS, over https, with redirects off.
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    ops::Range,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, FixedOffset, Utc};
use reqwest::{StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;

use crate::{inbox::Attachment, media::Blob};

/// Where handles are resolved and profiles read, and where `did:plc` documents live.
#[derive(Clone)]
pub struct Bluesky {
    pub appview: String,
    pub plc: String,
    /// Bluesky's video service, which takes uploads and makes the blob a post embeds.
    pub video: String,
    /// Tests only, never read from the environment: lets the PDS be a plain `http://127.0.0.1`
    /// mock. Names still resolve to public addresses only.
    pub allow_insecure_pds: bool,
    /// How long the inbox tick spends reading one account (`inbox::store_new`). Tests shorten it.
    pub inbox_budget: Duration,
    client: reqwest::Client,
    /// Inbox sessions by DID and password hash (see `live`).
    sessions: Arc<Mutex<HashMap<String, Live>>>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    InvalidHandle,
    /// The PDS is not a public https URL.
    UnsupportedPds,
    /// A wrong password, or the account password instead of an app password: one answer, so
    /// the endpoint does not tell them apart.
    InvalidCredentials,
    RateLimited,
    Unreachable,
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidHandle => "invalid_handle",
            Self::UnsupportedPds => "unsupported_pds",
            Self::InvalidCredentials => "invalid_credentials",
            Self::RateLimited => "rate_limited",
            Self::Unreachable => "bluesky_unreachable",
        }
    }
}

/// A verified account.
pub struct Account {
    pub did: String,
    pub handle: String,
    pub display_name: Option<String>,
    pub avatar: Option<String>,
    pub pds: String,
    /// The app password may use DMs.
    pub dm_access: bool,
}

/// A handle as typed (`@Alice.bsky.social`), trimmed and lowercased; `None` when it cannot be a
/// domain name.
pub fn handle(raw: &str) -> Option<String> {
    let h = raw.trim().trim_start_matches('@').to_ascii_lowercase();
    let label_ok = |l: &str| {
        (1..=63).contains(&l.len())
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !l.starts_with('-')
            && !l.ends_with('-')
    };
    (h.len() <= 253 && h.contains('.') && h.split('.').all(label_ok)).then_some(h)
}

/// Largest response body read from any Bluesky server.
const MAX_BODY: usize = 64 * 1024;

fn unreachable(e: impl std::fmt::Display, what: &str) -> Error {
    tracing::warn!(error = %e, what, "bluesky call failed");
    Error::Unreachable
}

/// Largest list (notifications, conversations, messages) read from Bluesky.
const MAX_LIST_BODY: usize = 1024 * 1024;

/// A request that never got a response: `unsupported_pds` when `PublicOnly` refused the host.
fn send_failed(e: reqwest::Error, what: &str) -> Error {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&e);
    while let Some(c) = cause {
        if c.is::<NotPublic>() || c.to_string() == NOT_PUBLIC {
            return Error::UnsupportedPds;
        }
        cause = c.source();
    }
    unreachable(e.without_url(), what)
}

enum Read {
    TooLarge,
    Failed(reqwest::Error),
}

/// The body, at most `cap` bytes (checked against `Content-Length`, then while reading).
async fn read_capped(mut res: reqwest::Response, cap: usize) -> Result<Vec<u8>, Read> {
    if res.content_length().is_some_and(|n| n > cap as u64) {
        return Err(Read::TooLarge);
    }
    let mut buf = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(Read::Failed)? {
        if buf.len() + chunk.len() > cap {
            return Err(Read::TooLarge);
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// The JSON body, read up to `MAX_BODY` bytes.
async fn json_body<T: DeserializeOwned>(res: reqwest::Response, what: &str) -> Result<T, Error> {
    json_capped(res, MAX_BODY, what).await
}

async fn json_capped<T: DeserializeOwned>(
    res: reqwest::Response,
    cap: usize,
    what: &str,
) -> Result<T, Error> {
    let buf = match read_capped(res, cap).await {
        Ok(buf) => buf,
        Err(Read::TooLarge) => return Err(unreachable("body over the cap", what)),
        Err(Read::Failed(e)) => return Err(unreachable(e.without_url(), what)),
    };
    serde_json::from_slice(&buf).map_err(|e| unreachable(e, what))
}

#[derive(Deserialize)]
struct HandleDid {
    did: String,
}

/// A handle's DID and PDS, confirmed both ways; no credentials used yet.
pub struct Identity {
    pub did: String,
    pds: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DidDoc {
    #[serde(default)]
    also_known_as: Vec<String>,
    #[serde(default)]
    service: Vec<Service>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Service {
    id: String,
    service_endpoint: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    did: String,
    access_jwt: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    display_name: Option<String>,
    avatar: Option<String>,
}

impl Bluesky {
    pub fn new(appview: &str, plc: &str) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            // A redirect could carry the password to another host.
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(Arc::new(PublicOnly))
            .user_agent("SocialReins")
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        Ok(Self {
            appview: appview.into(),
            plc: plc.into(),
            video: VIDEO_SERVICE.into(),
            allow_insecure_pds: false,
            inbox_budget: INBOX_BUDGET,
            client,
            sessions: Arc::default(),
        })
    }

    /// The DID and PDS of `handle` (already normalized by [`handle`]), with the DID document
    /// claiming the handle back.
    pub async fn resolve(&self, handle: &str) -> Result<Identity, Error> {
        let did = self.did(handle).await?;
        let pds = self.pds(&did, handle).await?;
        Ok(Identity { did, pds })
    }

    /// Checks `app_password` on the resolved account by opening a session on its PDS.
    pub async fn connect(
        &self,
        id: Identity,
        handle: &str,
        app_password: &str,
    ) -> Result<Account, Error> {
        let jwt = self.session(&id.pds, &id.did, app_password).await?;
        if full_access(&jwt) {
            return Err(Error::InvalidCredentials);
        }
        let profile = self.profile(&id.did).await;
        let dm_access = privileged(&jwt) && self.chat_ok(&id.pds, &jwt).await;
        Ok(Account {
            dm_access,
            // The handle confirmed both ways in `resolve`; the PDS's own claim is not trusted.
            handle: handle.to_owned(),
            display_name: profile.as_ref().and_then(|p| p.display_name.clone()),
            avatar: profile.and_then(|p| p.avatar),
            did: id.did,
            pds: id.pds,
        })
    }

    async fn did(&self, handle: &str) -> Result<String, Error> {
        let url = format!("{}/xrpc/com.atproto.identity.resolveHandle", self.appview);
        let res = self
            .client
            .get(url)
            .query(&[("handle", handle)])
            .send()
            .await
            .map_err(|e| send_failed(e, "resolveHandle"))?;
        match res.status() {
            StatusCode::OK => {}
            StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND => return Err(Error::InvalidHandle),
            s => return Err(unreachable(s, "resolveHandle")),
        }
        let did = json_body::<HandleDid>(res, "resolveHandle body").await?.did;
        let well_formed = |rest: &str| {
            !rest.is_empty()
                && rest
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        };
        match did.split_once(':').and_then(|(_, r)| r.split_once(':')) {
            Some(("plc" | "web", rest)) if did.starts_with("did:") && well_formed(rest) => Ok(did),
            _ => Err(Error::InvalidHandle),
        }
    }

    /// The PDS endpoint in the DID document: `plc.directory` for `did:plc`, the domain's
    /// `/.well-known/did.json` for `did:web`.
    async fn pds(&self, did: &str, handle: &str) -> Result<String, Error> {
        let url = match did.strip_prefix("did:web:") {
            Some(host) => format!(
                "{}/.well-known/did.json",
                self.checked_pds(&format!("https://{host}"))?
            ),
            None => format!("{}/{did}", self.plc),
        };
        let res = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| send_failed(e, "did document"))?;
        match res.status() {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND | StatusCode::GONE => return Err(Error::InvalidHandle),
            s => return Err(unreachable(s, "did document")),
        }
        let doc = json_body::<DidDoc>(res, "did document body").await?;
        let claimed = format!("at://{handle}");
        if !doc
            .also_known_as
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&claimed))
        {
            return Err(Error::InvalidHandle);
        }
        let endpoint = doc
            .service
            .iter()
            .find(|s| s.id == "#atproto_pds" || s.id == format!("{did}#atproto_pds"))
            .and_then(|s| s.service_endpoint.as_str())
            .ok_or(Error::InvalidHandle)?;
        self.checked_pds(endpoint)
    }

    /// `https://` on a DNS name, no credentials or query; tests may allow plain http on loopback.
    fn checked_pds(&self, endpoint: &str) -> Result<String, Error> {
        let url = Url::parse(endpoint).map_err(|_| Error::UnsupportedPds)?;
        let host = url.host_str().unwrap_or_default();
        let named = !host.is_empty()
            && host.trim_matches(['[', ']']).parse::<IpAddr>().is_err()
            && host != "localhost"
            && !host.ends_with(".localhost");
        let public = url.scheme() == "https" && named;
        let clean = url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none();
        if clean && (public || self.allow_insecure_pds) {
            Ok(url.as_str().trim_end_matches('/').to_owned())
        } else {
            Err(Error::UnsupportedPds)
        }
    }

    /// Opens a session on `pds` and returns its access token.
    async fn session(&self, pds: &str, did: &str, password: &str) -> Result<String, Error> {
        let url = format!("{pds}/xrpc/com.atproto.server.createSession");
        let body = serde_json::json!({"identifier": did, "password": password});
        let res = self
            .client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| send_failed(e, "createSession"))?;
        let status = res.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(Error::RateLimited);
        }
        // Includes `AuthFactorTokenRequired`, which only an account password with 2FA gets.
        if status == StatusCode::BAD_REQUEST || status == StatusCode::UNAUTHORIZED {
            return Err(Error::InvalidCredentials);
        }
        if status != StatusCode::OK {
            return Err(unreachable(status, "createSession"));
        }
        let session = json_body::<Session>(res, "createSession body").await?;
        if session.did != did {
            return Err(Error::InvalidCredentials);
        }
        Ok(session.access_jwt)
    }

    /// Display name and avatar from the public AppView; `None` when it has none to give, which
    /// does not stop the connect.
    async fn profile(&self, did: &str) -> Option<Profile> {
        let url = format!("{}/xrpc/app.bsky.actor.getProfile", self.appview);
        let res = self.client.get(url).query(&[("actor", did)]).send().await;
        match res {
            Ok(r) if r.status() == StatusCode::OK => json_body(r, "getProfile body").await.ok(),
            Ok(r) => {
                tracing::warn!(status = %r.status(), "bluesky profile missing");
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "bluesky profile failed");
                None
            }
        }
    }
}

const IMAGE_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/webp", "image/gif"];
const POST_COLLECTION: &str = "app.bsky.feed.post";
const VIDEO_SERVICE: &str = "https://video.bsky.app";
const VIDEO_UPLOAD_TIMEOUT: Duration = Duration::from_secs(150);
/// How long Bluesky gets to process an uploaded video.
const VIDEO_WAIT: Duration = Duration::from_secs(120);
const VIDEO_POLL: Duration = Duration::from_secs(2);
pub const MIXED_MEDIA: &str = "Bluesky takes up to 4 images or one video, not a mix.";
/// A post can be one upload and one wait, and the record after them.
pub const PUBLISH_TIMEOUT: Duration = Duration::from_secs(360);

/// Why a publish or takedown did not happen. Messages are safe to store and show: they never
/// hold a URL, token or secret.
#[derive(Debug, PartialEq, Eq)]
pub enum PostError {
    /// The app password was refused: the account must be connected again.
    Reconnect,
    /// The platform refused a permission, not the token: the user is told to reconnect (it may
    /// grant the permission), but nothing marks the account as dead.
    Denied,
    Failed(String),
}

fn failed(msg: impl Into<String>) -> PostError {
    PostError::Failed(msg.into())
}

/// An XRPC error name (`InvalidRequest`), kept only when it is a plain word, else the status.
fn xrpc_code(body: &Value, status: StatusCode) -> String {
    match body["error"].as_str() {
        Some(e)
            if !e.is_empty()
                && e.len() <= 40
                && e.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
        {
            e.to_owned()
        }
        _ => format!("HTTP {}", status.as_u16()),
    }
}

/// `https://bsky.app/profile/{did}/post/{rkey}` for a post's `at://` URI.
pub fn post_url(uri: &str) -> Option<String> {
    let (did, rkey) = post_key(uri)?;
    Some(format!("https://bsky.app/profile/{did}/post/{rkey}"))
}

/// The repo DID and record key of `at://{did}/app.bsky.feed.post/{rkey}`.
fn post_key(uri: &str) -> Option<(&str, &str)> {
    let rest = uri.strip_prefix("at://")?;
    let mut parts = rest.split('/');
    let (did, collection, rkey) = (parts.next()?, parts.next()?, parts.next()?);
    let plain = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '-' | '_' | '~'))
    };
    (parts.next().is_none() && collection == POST_COLLECTION && plain(did) && plain(rkey))
        .then_some((did, rkey))
}

/// The boxes named `name` directly in `data`, an MP4 or MOV box list; stops at a malformed one.
fn boxes<'a>(data: &'a [u8], name: &[u8; 4]) -> Vec<&'a [u8]> {
    let (mut found, mut rest) = (Vec::new(), data);
    while rest.len() >= 8 {
        let short = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        let (size, head) = match short {
            0 => (rest.len(), 8),
            1 if rest.len() >= 16 => {
                let wide: [u8; 8] = rest[8..16].try_into().unwrap_or_default();
                (usize::try_from(u64::from_be_bytes(wide)).unwrap_or(0), 16)
            }
            n => (n, 8),
        };
        if size < head || size > rest.len() {
            break;
        }
        if &rest[4..8] == name {
            found.push(&rest[head..size]);
        }
        rest = &rest[size..];
    }
    found
}

/// The largest `moov` box read to learn a video's shape; a longer one leaves the shape unsaid.
const MOOV_MAX: u64 = 16 * 1024 * 1024;
/// Bluesky's limit for one video.
const MAX_VIDEO_BYTES: u64 = 100_000_000;

/// The `moov` box of an MP4 or MOV, as the box list `mp4_size` reads, found by reading only the
/// header of each box before it (it often sits after the video data), so a stored video is not
/// read whole. `None` when there is none, the boxes are malformed, or it is over `MOOV_MAX`.
async fn moov(video: &Blob) -> Option<bytes::Bytes> {
    let mut at = 0u64;
    for _ in 0..64 {
        let head = video.range(at..at.checked_add(16)?).await.ok()?;
        let size = match u32::from_be_bytes(head.get(..4)?.try_into().ok()?) {
            0 => video.len() - at,
            1 => u64::from_be_bytes(head.get(8..16)?.try_into().ok()?),
            n => u64::from(n),
        };
        let end = at.checked_add(size).filter(|_| size >= 8)?;
        if head.get(4..8)? == b"moov" {
            return match size <= MOOV_MAX {
                true => video.range(at..end).await.ok(),
                false => None,
            };
        }
        at = end;
    }
    None
}

/// The display size of an MP4 or MOV's first video track, for the player's shape in the feed.
fn mp4_size(data: &[u8]) -> Option<(u32, u32)> {
    let word = |t: &[u8], at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(t.get(at..at + 4)?.try_into().ok()?))
    };
    let moov = *boxes(data, b"moov").first()?;
    boxes(moov, b"trak").into_iter().find_map(|trak| {
        let tkhd = *boxes(trak, b"tkhd").first()?;
        // The width sits at 76 in a version 0 `tkhd` and 88 in version 1, the matrix 36 before it.
        let at = if *tkhd.first()? == 1 { 88 } else { 76 };
        let (w, h) = (word(tkhd, at)? >> 16, word(tkhd, at + 4)? >> 16);
        // A quarter turn (phone video) has a zero first matrix entry.
        let turned = word(tkhd, at - 36)? == 0;
        (w > 0 && h > 0).then_some(if turned { (h, w) } else { (w, h) })
    })
}

impl Bluesky {
    /// POSTs an XRPC call with the session token and returns the JSON answer.
    async fn call(
        &self,
        pds: &str,
        jwt: &str,
        method: &str,
        body: reqwest::Body,
        content_type: &str,
    ) -> Result<Value, PostError> {
        let res = self
            .client
            .post(format!("{pds}/xrpc/{method}"))
            .bearer_auth(jwt)
            .header(reqwest::header::CONTENT_TYPE, content_type)
            .body(body)
            .send()
            .await
            .map_err(|e| {
                let sure = crate::connectors::never_sent(&e);
                tracing::warn!(error = %e.without_url(), method, "bluesky call failed");
                let msg = "Bluesky could not be reached.".to_owned();
                failed(crate::connectors::unsent(msg, sure))
            })?;
        let status = res.status();
        let answer: Value = json_body(res, method).await.unwrap_or_default();
        let code = xrpc_code(&answer, status);
        // An expired or revoked session answers 400 with these, as in `xrpc`.
        let expired = status == StatusCode::BAD_REQUEST
            && matches!(code.as_str(), "ExpiredToken" | "InvalidToken");
        match status {
            StatusCode::OK => Ok(answer),
            StatusCode::UNAUTHORIZED => Err(PostError::Reconnect),
            _ if expired => Err(PostError::Reconnect),
            StatusCode::TOO_MANY_REQUESTS => Err(failed(crate::connectors::unsent(
                "Bluesky is rate limiting this account.".into(),
                true,
            ))),
            _ => Err(failed(format!("Bluesky refused it ({code})."))),
        }
    }

    async fn call_json(
        &self,
        pds: &str,
        jwt: &str,
        method: &str,
        body: &Value,
    ) -> Result<Value, PostError> {
        let bytes = serde_json::to_vec(body).unwrap_or_default();
        self.call(pds, jwt, method, bytes.into(), "application/json")
            .await
    }

    /// Runs `call` with the account's session (cached, see `live`, so a thread logs in once).
    /// A cached session the PDS refuses (`401`, or `400 ExpiredToken`/`InvalidToken`) is opened
    /// again once: a refused session made nothing, so this cannot post twice.
    async fn with_session<T, F, Fut>(&self, c: &Creds<'_>, call: F) -> Result<T, PostError>
    where
        F: Fn(Live) -> Fut,
        Fut: std::future::Future<Output = Result<T, PostError>>,
    {
        for fresh in [false, true] {
            let s = self.live(c, fresh).await.map_err(|e| match e {
                InboxError::Reconnect => PostError::Reconnect,
                InboxError::RateLimited => failed(crate::connectors::unsent(
                    "Bluesky is rate limiting this account.".into(),
                    true,
                )),
                InboxError::Invalid(_) => failed("The account's Bluesky server is not supported."),
                _ => failed("Bluesky could not be reached."),
            })?;
            match call(s).await {
                Err(PostError::Reconnect) if !fresh => self.forget(c),
                done => return done,
            }
        }
        Err(PostError::Reconnect)
    }

    /// Publishes a post with up to 4 images or one video `(bytes, type, alt text)`, a reply when `reply` holds `(parent, root)`;
    /// returns its `(at:// uri, cid)`.
    pub async fn publish_post(
        &self,
        c: &Creds<'_>,
        text: &str,
        media: &[(&Blob, &str, &str)],
        reply: Option<(Ref<'_>, Ref<'_>)>,
    ) -> Result<(String, String), PostError> {
        self.with_session(c, |s| self.create_post(s, c.did, text, media, reply))
            .await
    }

    async fn create_post(
        &self,
        s: Live,
        did: &str,
        text: &str,
        media: &[(&Blob, &str, &str)],
        reply: Option<(Ref<'_>, Ref<'_>)>,
    ) -> Result<(String, String), PostError> {
        let (pds, jwt) = (&s.pds, &s.jwt);
        let mut record = post_record(text, &[], reply);
        if media.iter().any(|(_, mime, _)| mime.starts_with("video/")) {
            let [(bytes, mime, _)] = media else {
                return Err(failed(MIXED_MEDIA));
            };
            record["embed"] = self.video_embed(&s, did, bytes, mime).await?;
        } else {
            let mut images = Vec::new();
            for (i, (bytes, mime, alt)) in media.iter().enumerate() {
                if !IMAGE_TYPES.contains(mime) {
                    let n = i + 1;
                    return Err(failed(format!(
                        "Image {n} is not a JPEG, PNG, WebP or GIF."
                    )));
                }
                let up = self
                    .call(
                        pds,
                        jwt,
                        "com.atproto.repo.uploadBlob",
                        bytes.held()?.into(),
                        mime,
                    )
                    .await?;
                if !up["blob"].is_object() {
                    return Err(failed("Bluesky did not take an image."));
                }
                images.push(serde_json::json!({ "alt": alt, "image": up["blob"] }));
            }
            if !images.is_empty() {
                record["embed"] =
                    serde_json::json!({ "$type": "app.bsky.embed.images", "images": images });
            }
        }
        let body =
            serde_json::json!({ "repo": did, "collection": POST_COLLECTION, "record": record });
        let made = self
            .call_json(pds, jwt, "com.atproto.repo.createRecord", &body)
            .await?;
        match (made["uri"].as_str(), made["cid"].as_str()) {
            (Some(uri), Some(cid)) if post_key(uri).is_some() => {
                Ok((uri.to_owned(), cid.to_owned()))
            }
            _ => Err(failed("Bluesky did not return the post's id.")),
        }
    }

    /// Uploads a video to Bluesky's video service, waits until it is processed and returns the
    /// post embed. The service takes a short-lived token the PDS signs for it, never the session.
    /// The video is one request body, streamed from the store with its `Content-Length`.
    async fn video_embed(
        &self,
        s: &Live,
        did: &str,
        bytes: &Blob,
        mime: &str,
    ) -> Result<Value, PostError> {
        if bytes.len() > MAX_VIDEO_BYTES {
            return Err(failed("The video is over 100 MB, Bluesky's limit."));
        }
        let host = Url::parse(&s.pds)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .ok_or_else(|| failed("Bluesky could not be reached."))?;
        let aud = format!("did:web:{host}");
        let exp = (Utc::now().timestamp() + 1800).to_string();
        let query = [
            ("aud", aud.as_str()),
            ("lxm", "com.atproto.repo.uploadBlob"),
            ("exp", exp.as_str()),
        ];
        let unreachable = |e: reqwest::Error| {
            tracing::warn!(error = %e.without_url(), "bluesky video call failed");
            failed("Bluesky could not be reached.")
        };
        let res = self
            .client
            .get(format!("{}/xrpc/com.atproto.server.getServiceAuth", s.pds))
            .bearer_auth(&s.jwt)
            .query(&query)
            .send()
            .await
            .map_err(unreachable)?;
        let status = res.status();
        let auth: Value = json_body(res, "getServiceAuth").await.unwrap_or_default();
        let Some(token) = auth["token"].as_str().filter(|_| status == StatusCode::OK) else {
            let code = xrpc_code(&auth, status);
            // An expired or revoked session answers 400 with these, as in `call`.
            let expired = status == StatusCode::BAD_REQUEST
                && matches!(code.as_str(), "ExpiredToken" | "InvalidToken");
            return Err(if status == StatusCode::UNAUTHORIZED || expired {
                PostError::Reconnect
            } else {
                failed(format!("Bluesky refused the video upload ({code})."))
            });
        };
        let res = self
            .client
            .post(format!("{}/xrpc/app.bsky.video.uploadVideo", self.video))
            .bearer_auth(token)
            .query(&[("did", did), ("name", "video.mp4")])
            .header(reqwest::header::CONTENT_TYPE, mime)
            .header(reqwest::header::CONTENT_LENGTH, bytes.len())
            .timeout(VIDEO_UPLOAD_TIMEOUT)
            .body(bytes.body().await?)
            .send()
            .await
            .map_err(unreachable)?;
        let status = res.status();
        let up: Value = json_body(res, "uploadVideo").await.unwrap_or_default();
        // The service answers the job flat, and 409 with the same job for a video it already has.
        let job = if up["jobStatus"].is_object() {
            &up["jobStatus"]
        } else {
            &up
        };
        let known = status.is_success() || status == StatusCode::CONFLICT;
        let blob = if job["blob"].is_object() {
            job["blob"].clone()
        } else if let Some(job_id) = job["jobId"].as_str().filter(|_| known) {
            self.finished_video(job_id).await?
        } else {
            return Err(failed(match status {
                StatusCode::TOO_MANY_REQUESTS => {
                    "Bluesky is rate limiting this account.".to_owned()
                }
                _ => format!("Bluesky refused the video ({}).", xrpc_code(&up, status)),
            }));
        };
        let mut embed = serde_json::json!({ "$type": "app.bsky.embed.video", "video": blob });
        let moov = moov(bytes).await.unwrap_or_default();
        if let Some((width, height)) = mp4_size(&moov) {
            embed["aspectRatio"] = serde_json::json!({ "width": width, "height": height });
        }
        Ok(embed)
    }

    /// The blob of a video job, once Bluesky has processed it.
    async fn finished_video(&self, job_id: &str) -> Result<Value, PostError> {
        let url = format!("{}/xrpc/app.bsky.video.getJobStatus", self.video);
        let deadline = std::time::Instant::now() + VIDEO_WAIT;
        loop {
            let res = self
                .client
                .get(&url)
                .query(&[("jobId", job_id)])
                .send()
                .await;
            // A failed poll says nothing about the job: only the deadline ends the wait.
            let status: Value = match res {
                Ok(res) => json_body(res, "getJobStatus").await.unwrap_or_default(),
                Err(_) => Value::Null,
            };
            let job = if status["jobStatus"].is_object() {
                &status["jobStatus"]
            } else {
                &status
            };
            // A video Bluesky already has ends as a failed job that still carries its blob.
            if job["blob"].is_object() {
                return Ok(job["blob"].clone());
            }
            match job["state"].as_str() {
                Some("JOB_STATE_COMPLETED") => {
                    return Err(failed("Bluesky did not return the processed video."))
                }
                Some("JOB_STATE_FAILED") => {
                    return Err(failed("Bluesky could not process the video."))
                }
                _ if std::time::Instant::now() >= deadline => {
                    return Err(failed("Bluesky took too long to process the video."))
                }
                _ => tokio::time::sleep(VIDEO_POLL).await,
            }
        }
    }

    /// Deletes the post at `uri` from the account's repo.
    pub async fn delete_post(&self, c: &Creds<'_>, uri: &str) -> Result<(), PostError> {
        let Some((repo, rkey)) = post_key(uri).filter(|(repo, _)| *repo == c.did) else {
            return Err(failed("This post does not belong to the account."));
        };
        let body = serde_json::json!({ "repo": repo, "collection": POST_COLLECTION, "rkey": rkey });
        let body = &body;
        self.with_session(c, |s| async move {
            self.call_json(&s.pds, &s.jwt, "com.atproto.repo.deleteRecord", body)
                .await
                .map(|_| ())
        })
        .await
    }
}

/// Chat calls go to the account's PDS, which forwards them to Bluesky's chat service.
const CHAT_PROXY: &str = "did:web:api.bsky.chat#bsky_chat";
/// Ingest reads at most this many notifications, and messages of this many conversations, a
/// round; a first round reads back at most `inbox::FIRST_DAYS`.
const MAX_NOTIFICATIONS: usize = 300;
/// A full round is 3 pages of 100; a few more allow for short pages.
const MAX_NOTIFICATION_PAGES: usize = 6;
const MAX_CONVOS: usize = 20;
/// Time the inbox tick gives one account: a read that runs out stops at the next page or
/// conversation (so it can overrun by one call) and carries on next tick.
const INBOX_BUDGET: Duration = Duration::from_secs(20);

/// Why an inbox call failed. `code` is stored on automation runs and sent to the API.
#[derive(Debug, PartialEq, Eq)]
pub enum InboxError {
    /// The app password was refused: the account must be connected again.
    Reconnect,
    /// The app password was made without DM access.
    NoDmAccess,
    /// The recipient's settings do not take DMs from this account.
    DmNotAllowed,
    RateLimited,
    /// The post's author blocks this account: a reply would be hidden.
    BlockedByAuthor,
    /// The post's thread does not take replies from this account (its threadgate).
    RepliesDisabled,
    /// The post is gone, or Bluesky does not show it to this account.
    PostGone,
    /// Bluesky answered with an error; `code` is a scrubbed XRPC error name.
    Refused {
        status: u16,
        code: String,
    },
    Unreachable,
    /// Not sent, for a reason found before calling (e.g. `unsupported_pds`, `too_long`).
    Invalid(&'static str),
    /// Meta refused the Page token, or a permission was not granted: connect the account again.
    MetaReconnect,
    /// Meta refused the call, or could not be reached; the scrubbed reason.
    Meta(String),
    /// Meta refused it for a permission or a messaging policy (codes 10 and 200 to 299); the
    /// scrubbed reason.
    MetaDenied(String),
    /// X refused the token or a permission is missing: connect the account again.
    XReconnect,
    /// X refused the call, or could not be reached; what to tell a person (`connectors::x::inbox`).
    X(String),
    /// Threads refused the token: connect the account again.
    ThreadsReconnect,
    /// Threads refused a permission (codes 10 and 200 to 299): the account was connected before
    /// replies existed, or without the permission; connecting it again asks for it.
    ThreadsDenied,
    /// Threads refused the call, or could not be reached; the scrubbed reason.
    Threads(String),
    /// YouTube refused the channel's token or a permission is missing: connect the channel again.
    YouTubeReconnect,
    /// YouTube refused the call, or could not be reached; what to tell a person.
    YouTube(String),
}

impl InboxError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Reconnect => "reconnect_required",
            Self::NoDmAccess => "no_dm_access",
            Self::DmNotAllowed => "dm_not_allowed",
            Self::RateLimited => "rate_limited",
            Self::BlockedByAuthor => "blocked_by_author",
            Self::RepliesDisabled => "replies_disabled",
            Self::PostGone => "post_unavailable",
            Self::Refused { .. } => "bluesky_refused",
            Self::Unreachable => "bluesky_unreachable",
            Self::Invalid(code) => code,
            // Not `reconnect_required`: the web app words that one for Bluesky.
            Self::MetaReconnect => "meta_reconnect_required",
            Self::Meta(_) => "meta_failed",
            Self::MetaDenied(_) => "meta_denied",
            Self::XReconnect => "x_reconnect_required",
            Self::X(_) => "x_failed",
            Self::ThreadsReconnect => "threads_reconnect_required",
            Self::ThreadsDenied => "threads_permission_missing",
            Self::Threads(_) => "threads_failed",
            Self::YouTubeReconnect => "youtube_reconnect_required",
            Self::YouTube(_) => "youtube_failed",
        }
    }

    /// What to tell a person; never holds a URL, token or secret.
    pub fn message(&self) -> String {
        match self {
            Self::Reconnect => {
                "Bluesky refused the app password. Reconnect the account under Connections.".into()
            }
            Self::NoDmAccess => "The app password has no DM access. Make one with \"Allow access \
                                 to your direct messages\" and reconnect the account."
                .into(),
            Self::DmNotAllowed => "The DM was not taken: the recipient's settings do not allow \
                                   messages from this account."
                .into(),
            Self::RateLimited => "The platform is rate limiting this account.".into(),
            Self::BlockedByAuthor => "The author of this post blocks this account, so a reply \
                                      would not show. Nothing was sent."
                .into(),
            Self::RepliesDisabled => "This thread does not take replies from this account \
                                      (its author limited who can reply). Nothing was sent."
                .into(),
            Self::PostGone => "The post was deleted, or Bluesky does not show it to this \
                               account. Nothing was sent."
                .into(),
            Self::Refused { code, .. } => format!("Bluesky refused it ({code})."),
            Self::Unreachable => "Bluesky could not be reached.".into(),
            Self::Invalid("outside_window") => "Meta only takes this message within 24 hours of \
                                                the person's last message, or 7 days of their \
                                                comment."
                .into(),
            Self::Invalid("already_replied") => "Meta takes one private reply per comment, and \
                                                 this one has it."
                .into(),
            Self::Invalid("dm_unavailable") => {
                "Meta cannot deliver a message to this person.".into()
            }
            Self::Invalid("not_found") => {
                "It is gone on Facebook or Instagram (deleted, or the person removed it).".into()
            }
            Self::Invalid("not_supported") => "Facebook and Instagram do not allow this on \
                                               this item."
                .into(),
            Self::Invalid("not_applicable") => "That does not apply to this item.".into(),
            Self::Invalid("own_account") => "That is one of your own accounts.".into(),
            Self::Invalid("not_our_thread") => "Only replies under a post of this account can \
                                                be hidden."
                .into(),
            Self::Invalid("too_many_hidden") => "Bluesky keeps at most 300 hidden replies on \
                                                 a post."
                .into(),
            Self::Invalid("threads_replies_closed") => {
                "The author of this post limited who can reply \
                                                        to it, so a reply from this account was \
                                                        not sent."
                    .into()
            }
            Self::Invalid("threads_no_dm") => "Threads has no direct messages, so nothing was \
                                               sent."
                .into(),
            Self::Invalid("threads_no_action") => {
                "Threads does not let apps like, repost, follow, \
                                                   mute or block, so nothing was done."
                    .into()
            }
            Self::Invalid("threads_nested") => "Threads can only hide a reply directly under a \
                                                post of this account, not one deeper in the \
                                                thread."
                .into(),
            Self::Invalid("invalid_emoji") => "A reaction is one emoji.".into(),
            Self::Invalid("convo_locked") => "This conversation is locked: no one can send \
                                              messages in it now."
                .into(),
            Self::Invalid("invalid_reason") => "Pick a reason for the report (spam, violation, \
                                                misleading, sexual, rude or other); what you add \
                                                takes up to 2000 characters."
                .into(),
            Self::Invalid("youtube_no_dm") => "YouTube has no direct messages and no private \
                                               replies. Only a public reply works."
                .into(),
            Self::Invalid("reply_not_found") => "That reply is not on this comment.".into(),
            Self::Invalid("youtube_write_budget") => "This channel used the 40 replies and \
                                                     moderation calls it may make a day (each \
                                                     costs 50 of YouTube's quota units). Try \
                                                     again after midnight Pacific time."
                .into(),
            Self::Invalid("rejected") => "This comment was rejected, so it takes no reply.".into(),
            Self::Invalid(code) => format!("Not sent ({code})."),
            Self::MetaReconnect => "Meta refused the Page's token or a permission is missing. \
                                    Connect the account again under Connections."
                .into(),
            Self::Meta(why) => format!("Meta did not take it: {why}"),
            Self::MetaDenied(why) => format!(
                "Meta did not allow it: {why} The account may lack a permission for this, or \
                 Meta's messaging rules forbid it."
            ),
            Self::XReconnect => "X refused the account's token or a permission is missing. \
                                 Connect the account again under Connections."
                .into(),
            Self::X(why) => why.clone(),
            Self::ThreadsReconnect => "Threads refused the account's token (it expired or was \
                                       revoked). Connect the account again under Connections."
                .into(),
            Self::ThreadsDenied => "Threads did not allow this: the account was connected \
                                    without the permission for replies. Connect it again under \
                                    Connections and accept the new permissions."
                .into(),
            Self::Threads(why) => format!("Threads did not take it: {why}"),
            Self::YouTubeReconnect => "YouTube refused the channel's token or a permission is \
                                       missing. Connect the channel again under Accounts."
                .into(),
            Self::YouTube(why) => why.clone(),
        }
    }
}

/// An open session, kept until 5 minutes before it expires: polling every minute must not open
/// a session each time (Bluesky allows 300 a day per account).
#[derive(Clone)]
struct Live {
    pds: String,
    jwt: String,
    exp: i64,
    privileged: bool,
}

/// An account's PDS (`meta.pds`, checked again), DID and app password.
pub struct Creds<'a> {
    pub pds: &'a str,
    pub did: &'a str,
    pub password: &'a str,
}

/// Someone on Bluesky, or (as an inbox item's author) on Facebook or Instagram: `did` is then
/// their id there.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Author {
    pub did: String,
    pub handle: Option<String>,
    pub display_name: Option<String>,
    pub avatar: Option<String>,
}

fn author(v: &Value) -> Option<Author> {
    let text = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    Some(Author {
        did: text("did")?,
        handle: text("handle"),
        display_name: text("displayName"),
        avatar: text("avatar"),
    })
}

/// A reply to the account (`comment`), a mention or quote of it (`mention`), or activity that
/// needs no answer: a new follower (`follow`: the follow record), a like or a repost of one of
/// its posts (`like`, `repost`: `parent_uri` is that post). Activity has no text.
#[derive(Debug, PartialEq, Eq)]
pub struct Notification {
    pub kind: &'static str,
    pub uri: String,
    pub cid: Option<String>,
    pub author: Author,
    pub text: String,
    pub parent_uri: Option<String>,
    /// The thread root; the post itself when it is not a reply.
    pub root: (String, Option<String>),
    pub indexed_at: DateTime<FixedOffset>,
    /// Images, a video or a link card the post embeds.
    pub attachments: Vec<Attachment>,
    /// Where to look at it: the liked or reposted post for `like` and `repost`.
    pub permalink: Option<String>,
}

/// A blob's cid, when it is one: letters and digits only, so it is safe in a CDN path.
fn blob_cid(b: &Value) -> Option<&str> {
    let cid = b["ref"]["$link"].as_str().or(b["cid"].as_str())?;
    let ok = !cid.is_empty() && cid.len() <= 128 && cid.chars().all(|c| c.is_ascii_alphanumeric());
    ok.then_some(cid)
}

/// What a post record's `embed` shows, as Bluesky's CDN serves it for `did`'s blobs.
/// `post` is the post's page on bsky.app, which a video opens (its stream does not play in a tab).
fn embed_attachments(did: &str, embed: &Value, post: Option<&str>) -> Vec<Attachment> {
    let did_ok = did.starts_with("did:")
        && did
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ":._-".contains(c));
    if !did_ok {
        return Vec::new();
    }
    let image = |cid: &str| {
        let full = format!("https://cdn.bsky.app/img/feed_fullsize/plain/{did}/{cid}@jpeg");
        let thumb = format!("https://cdn.bsky.app/img/feed_thumbnail/plain/{did}/{cid}@jpeg");
        Attachment::new("image", Some(&full), Some(&thumb))
    };
    match embed["$type"].as_str().unwrap_or_default() {
        "app.bsky.embed.images" => embed["images"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| image(blob_cid(&i["image"])?))
            .collect(),
        "app.bsky.embed.video" => blob_cid(&embed["video"])
            .and_then(|cid| {
                // The stream is HLS, which a browser tab does not play: the still stands for it.
                let thumb = format!("https://video.bsky.app/watch/{did}/{cid}/thumbnail.jpg");
                Attachment::new("video", Some(post.unwrap_or(&thumb)), Some(&thumb))
            })
            .into_iter()
            .collect(),
        "app.bsky.embed.external" => {
            let ext = &embed["external"];
            let thumb = blob_cid(&ext["thumb"]).map(|cid| {
                format!("https://cdn.bsky.app/img/feed_thumbnail/plain/{did}/{cid}@jpeg")
            });
            Attachment::new("link", ext["uri"].as_str(), thumb.as_deref())
                .into_iter()
                .collect()
        }
        "app.bsky.embed.recordWithMedia" => embed_attachments(did, &embed["media"], post),
        _ => Vec::new(),
    }
}

fn time(v: &Value) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(v.as_str()?).ok()
}

/// One `listNotifications` entry; `None` for other reasons or a malformed entry.
pub fn notification(n: &Value) -> Option<Notification> {
    let kind = match n["reason"].as_str()? {
        "reply" => "comment",
        "mention" | "quote" => "mention",
        "follow" => "follow",
        "like" => "like",
        "repost" => "repost",
        _ => return None,
    };
    let uri = n["uri"].as_str()?.to_owned();
    let cid = n["cid"].as_str().map(str::to_owned);
    let reply = &n["record"]["reply"];
    let liked = matches!(kind, "like" | "repost")
        .then(|| n["reasonSubject"].as_str())
        .flatten()
        .filter(|u| post_key(u).is_some());
    // A like of something that is not a post is no activity on one of ours.
    if matches!(kind, "like" | "repost") && liked.is_none() {
        return None;
    }
    let root = match (liked, reply["root"]["uri"].as_str()) {
        (Some(post), _) => (
            post.to_owned(),
            n["record"]["subject"]["cid"].as_str().map(str::to_owned),
        ),
        (None, Some(r)) => (
            r.to_owned(),
            reply["root"]["cid"].as_str().map(str::to_owned),
        ),
        (None, None) => (uri.clone(), cid.clone()),
    };
    let author = author(&n["author"])?;
    Some(Notification {
        permalink: liked.and_then(post_url),
        parent_uri: liked.or(reply["parent"]["uri"].as_str()).map(str::to_owned),
        kind,
        cid,
        attachments: embed_attachments(
            &author.did,
            &n["record"]["embed"],
            post_url(&uri).as_deref(),
        ),
        author,
        text: n["record"]["text"].as_str().unwrap_or_default().to_owned(),
        root,
        indexed_at: time(&n["indexedAt"])?,
        uri,
    })
}

/// A post in the thread above an inbox item.
#[derive(Debug, PartialEq, Eq)]
pub struct ThreadPost {
    pub uri: String,
    pub author: Author,
    pub text: String,
    pub at: Option<DateTime<FixedOffset>>,
    pub attachments: Vec<Attachment>,
}

/// The posts above a `getPostThread` answer's own post, oldest first, and the post itself last
/// when `with_post`.
#[derive(Debug, PartialEq, Eq)]
pub struct Thread {
    pub posts: Vec<ThreadPost>,
    /// The chain stops at a post that is deleted or hidden from the account.
    pub truncated: bool,
}

/// Walks `parent` links from a `threadViewPost`, at most 25 up (what was asked for).
pub fn thread_above(node: &Value, with_post: bool) -> Thread {
    let mut posts = Vec::new();
    let mut truncated = false;
    let mut cur = if with_post { node } else { &node["parent"] };
    while !cur.is_null() && posts.len() < 26 {
        if cur["$type"] != "app.bsky.feed.defs#threadViewPost" {
            truncated = true;
            break;
        }
        let post = &cur["post"];
        if let (Some(uri), Some(who)) = (post["uri"].as_str(), author(&post["author"])) {
            posts.push(ThreadPost {
                attachments: embed_attachments(
                    &who.did,
                    &post["record"]["embed"],
                    post_url(uri).as_deref(),
                ),
                text: post["record"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                at: time(&post["record"]["createdAt"]).or_else(|| time(&post["indexedAt"])),
                uri: uri.to_owned(),
                author: who,
            });
        }
        cur = &cur["parent"];
    }
    posts.reverse();
    Thread { posts, truncated }
}

/// A DM someone sent the account.
#[derive(Debug, PartialEq, Eq)]
pub struct Message {
    pub convo_id: String,
    pub id: String,
    pub sender: Author,
    pub text: String,
    pub sent_at: DateTime<FixedOffset>,
}

/// What `Bluesky::notifications` read. `next` is the cursor of the rest, when it stopped early.
#[derive(Debug)]
pub struct Notifications {
    pub found: Vec<Notification>,
    pub next: Option<String>,
    /// Why a later page failed: `found` keeps what the earlier pages gave and `next` is the
    /// page that failed, so the next tick carries on from it.
    pub stopped: Option<InboxError>,
}

/// What `Bluesky::messages` read. `resume` is set when it stopped early (`newest` is then
/// `None`); otherwise `newest` is the next `since`.
#[derive(Debug)]
pub struct Messages {
    pub found: Vec<Message>,
    pub newest: Option<DateTime<FixedOffset>>,
    pub resume: Option<DateTime<FixedOffset>>,
    /// Why a later conversation failed: `found` keeps what the earlier ones gave and `resume`
    /// is the last one read.
    pub stopped: Option<InboxError>,
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

fn login_error(e: Error) -> InboxError {
    match e {
        Error::InvalidCredentials => InboxError::Reconnect,
        Error::RateLimited => InboxError::RateLimited,
        Error::UnsupportedPds => InboxError::Invalid("unsupported_pds"),
        Error::InvalidHandle | Error::Unreachable => InboxError::Unreachable,
    }
}

impl Bluesky {
    /// A cached session for `c`, or a new one when `fresh` or none is left.
    async fn live(&self, c: &Creds<'_>, fresh: bool) -> Result<Live, InboxError> {
        let key = format!("{}:{}", c.did, socialreins_core::token::hash(c.password));
        let now = now_secs();
        if !fresh {
            let cached = self.sessions.lock().ok().and_then(|m| m.get(&key).cloned());
            if let Some(s) = cached.filter(|s| s.exp > now + 300) {
                return Ok(s);
            }
        }
        let pds = self.checked_pds(c.pds).map_err(login_error)?;
        let jwt = self
            .session(&pds, c.did, c.password)
            .await
            .map_err(login_error)?;
        let exp = claims(&jwt)
            .and_then(|c| c["exp"].as_i64())
            .unwrap_or(now + 600);
        let live = Live {
            privileged: privileged(&jwt),
            pds,
            jwt,
            exp,
        };
        if let Ok(mut m) = self.sessions.lock() {
            m.retain(|_, s| s.exp > now);
            m.insert(key, live.clone());
        }
        Ok(live)
    }

    fn forget(&self, c: &Creds<'_>) {
        let key = format!("{}:{}", c.did, socialreins_core::token::hash(c.password));
        if let Ok(mut m) = self.sessions.lock() {
            m.remove(&key);
        }
    }

    /// An XRPC call on the account's PDS: a GET with `query`, or a POST of `body`. `chat.*`
    /// methods go through the chat proxy and need DM access. A rejected session is opened again
    /// once: the call never ran, so this cannot send twice.
    pub(crate) async fn xrpc(
        &self,
        c: &Creds<'_>,
        method: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Value, InboxError> {
        self.xrpc_via(c, method, query, body, None).await
    }

    /// `xrpc`, with `proxy` (a service DID and id) as the `atproto-proxy` the PDS forwards to,
    /// for calls a moderation service takes. `chat.*` always goes to the chat service.
    pub(crate) async fn xrpc_via(
        &self,
        c: &Creds<'_>,
        method: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
        proxy: Option<&str>,
    ) -> Result<Value, InboxError> {
        let chat = method.starts_with("chat.");
        for fresh in [false, true] {
            let s = self.live(c, fresh).await?;
            if chat && !s.privileged {
                return Err(InboxError::NoDmAccess);
            }
            let url = format!("{}/xrpc/{method}", s.pds);
            let mut req = match body {
                Some(b) => self.client.post(url).json(b),
                None => self.client.get(url).query(query),
            };
            req = req.bearer_auth(&s.jwt);
            if let Some(proxy) = proxy.or(chat.then_some(CHAT_PROXY)) {
                req = req.header("atproto-proxy", proxy);
            }
            let res = req.send().await.map_err(|e| {
                tracing::warn!(error = %e.without_url(), method, "bluesky call failed");
                InboxError::Unreachable
            })?;
            let status = res.status();
            let answer: Value = json_capped(res, MAX_LIST_BODY, method)
                .await
                .unwrap_or_default();
            let code = xrpc_code(&answer, status);
            let expired = status == StatusCode::UNAUTHORIZED
                || (status == StatusCode::BAD_REQUEST
                    && matches!(code.as_str(), "ExpiredToken" | "InvalidToken"));
            match status {
                StatusCode::OK => return Ok(answer),
                _ if expired => self.forget(c),
                StatusCode::TOO_MANY_REQUESTS => return Err(InboxError::RateLimited),
                s => {
                    return Err(InboxError::Refused {
                        status: s.as_u16(),
                        code,
                    })
                }
            }
        }
        Err(InboxError::Reconnect)
    }

    /// The posts `uris` (at most 25, `app.bsky.feed.getPosts`) with their counts, as the account
    /// sees them. A post that is gone is left out.
    pub async fn posts(&self, c: &Creds<'_>, uris: &[&str]) -> Result<Vec<Value>, InboxError> {
        let query: Vec<(&str, &str)> = uris.iter().map(|u| ("uris", *u)).collect();
        let got = self.xrpc(c, "app.bsky.feed.getPosts", &query, None).await?;
        Ok(got["posts"].as_array().cloned().unwrap_or_default())
    }

    /// The posts above `uri` in its thread, oldest first, as `account` sees them. `truncated`
    /// when the chain ends at a post that is gone or hidden from the account. `PostGone` when
    /// `uri` itself is gone.
    pub async fn thread_above(
        &self,
        c: &Creds<'_>,
        uri: &str,
        with_post: bool,
    ) -> Result<Thread, InboxError> {
        let q = [("uri", uri), ("depth", "0"), ("parentHeight", "25")];
        let got = self
            .xrpc(c, "app.bsky.feed.getPostThread", &q, None)
            .await
            .map_err(|e| match e {
                InboxError::Refused { ref code, .. } if code == "NotFound" => InboxError::PostGone,
                e => e,
            })?;
        // The post itself is blocked, hidden or gone: nothing to show.
        if got["thread"]["$type"] != "app.bsky.feed.defs#threadViewPost" {
            return Err(InboxError::PostGone);
        }
        Ok(thread_above(&got["thread"], with_post))
    }

    /// Whether a reply to `post` would show: Bluesky still serves it, its author does not block
    /// this account and its thread takes replies from it. Nothing is sent either way. A check
    /// that cannot be made (an outage, an unknown post view) lets the reply try.
    pub async fn can_reply(&self, c: &Creds<'_>, post: &str) -> Result<(), InboxError> {
        let got = match self.posts(c, &[post]).await {
            Ok(got) => got,
            Err(e @ (InboxError::Reconnect | InboxError::RateLimited)) => return Err(e),
            Err(_) => return Ok(()),
        };
        let Some(p) = got.first() else {
            return Err(InboxError::PostGone);
        };
        if p["author"]["viewer"]["blockedBy"] == true {
            return Err(InboxError::BlockedByAuthor);
        }
        if p["viewer"]["replyDisabled"] == true {
            return Err(InboxError::RepliesDisabled);
        }
        Ok(())
    }

    /// Replies, mentions and follows newer than `since`, newest first, from the page `resume`
    /// names (none: the newest). Stops after `MAX_NOTIFICATIONS` items, `MAX_NOTIFICATION_PAGES`
    /// pages or when `deadline` has passed (always after at least one page), and then gives the
    /// cursor of the rest in `next`. Never marks them read.
    pub async fn notifications(
        &self,
        c: &Creds<'_>,
        since: DateTime<FixedOffset>,
        resume: Option<&str>,
        deadline: Instant,
    ) -> Result<Notifications, InboxError> {
        let mut found = Vec::new();
        let mut cursor = resume.map(str::to_owned);
        let (mut read, mut pages) = (0, 0);
        loop {
            let mut query = vec![
                ("limit", "100"),
                ("reasons", "reply"),
                ("reasons", "mention"),
                ("reasons", "quote"),
                ("reasons", "follow"),
                ("reasons", "like"),
                ("reasons", "repost"),
            ];
            if let Some(c) = &cursor {
                query.push(("cursor", c));
            }
            let page = match self
                .xrpc(c, "app.bsky.notification.listNotifications", &query, None)
                .await
            {
                Ok(page) => page,
                // A later page that fails (429, 5xx) must not drop the earlier ones.
                Err(e) if pages > 0 => {
                    return Ok(Notifications {
                        found,
                        next: cursor,
                        stopped: Some(e),
                    })
                }
                Err(e) => return Err(e),
            };
            let list = page["notifications"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            (read, pages) = (read + list.len(), pages + 1);
            for n in &list {
                if time(&n["indexedAt"]).is_some_and(|t| t <= since) {
                    return Ok(Notifications {
                        found,
                        next: None,
                        stopped: None,
                    });
                }
                found.extend(notification(n));
            }
            let before = cursor.take();
            cursor = page["cursor"].as_str().map(str::to_owned);
            // A cursor that does not move would keep the round open for ever.
            if list.is_empty() || cursor.is_none() || cursor == before {
                return Ok(Notifications {
                    found,
                    next: None,
                    stopped: None,
                });
            }
            // Counted in pages and time too: a server the account owner runs could answer one
            // item a page, slowly, for ever, and the inbox tick waits on every account.
            if read >= MAX_NOTIFICATIONS
                || pages >= MAX_NOTIFICATION_PAGES
                || Instant::now() >= deadline
            {
                return Ok(Notifications {
                    found,
                    next: cursor,
                    stopped: None,
                });
            }
        }
    }

    /// Whether the account can use DMs: a privileged app password that the chat service takes.
    /// An outage is `Err`, so a caller can keep what it knew.
    pub async fn check_dm_access(&self, c: &Creds<'_>) -> Result<bool, InboxError> {
        let q = [("limit", "1")];
        match self.xrpc(c, "chat.bsky.convo.listConvos", &q, None).await {
            Ok(_) => Ok(true),
            Err(InboxError::NoDmAccess | InboxError::Refused { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// `listConvos` with a token from `connect`, for the connection's `dm_access`.
    async fn chat_ok(&self, pds: &str, jwt: &str) -> bool {
        let url = format!("{pds}/xrpc/chat.bsky.convo.listConvos");
        let res = self
            .client
            .get(url)
            .query(&[("limit", "1")])
            .bearer_auth(jwt)
            .header("atproto-proxy", CHAT_PROXY)
            .send()
            .await;
        matches!(res, Ok(r) if r.status() == StatusCode::OK)
    }

    /// DMs others sent the account after `since`. Reads the 50 latest conversations, those with
    /// news oldest first, and stops after `MAX_CONVOS` of them or when `deadline` has passed
    /// (always after at least one): `resume` is then the time of the last one read, which the
    /// next call passes back to skip what is done. A round that reaches the end gives `newest`,
    /// the next `since`.
    pub async fn messages(
        &self,
        c: &Creds<'_>,
        since: DateTime<FixedOffset>,
        resume: Option<DateTime<FixedOffset>>,
        deadline: Instant,
    ) -> Result<Messages, InboxError> {
        let q = [("limit", "50")];
        let page = self.xrpc(c, "chat.bsky.convo.listConvos", &q, None).await?;
        let mut convos = page["convos"].as_array().cloned().unwrap_or_default();
        // Message requests (people the account does not follow, who may write to it) are listed
        // apart from the accepted conversations.
        let mut requests_missed = false;
        let asked = self
            .xrpc(c, "chat.bsky.convo.listConvoRequests", &q, None)
            .await;
        match asked {
            Ok(req) => {
                for r in req["requests"].as_array().into_iter().flatten() {
                    let fresh = r["id"]
                        .as_str()
                        .is_some_and(|id| !convos.iter().any(|k| k["id"].as_str() == Some(id)));
                    // Group join requests the account made are not conversations to read.
                    if fresh && r["lastMessage"].is_object() {
                        convos.push(r.clone());
                    }
                }
            }
            Err(e @ (InboxError::Reconnect | InboxError::RateLimited)) => return Err(e),
            // The service is down or timing out: the requests may be missing from this round.
            Err(InboxError::Unreachable) | Err(InboxError::Refused { status: 500.., .. }) => {
                requests_missed = true
            }
            // Refused or unknown (an older server): there are no requests to read.
            Err(_) => {}
        }
        let mut fresh: Vec<_> = convos
            .iter()
            .filter_map(|convo| {
                let last = time(&convo["lastMessage"]["sentAt"])?;
                (last > since).then_some((convo, last))
            })
            .collect();
        fresh.sort_by_key(|(_, last)| *last);
        let newest = fresh.last().map(|(_, last)| *last);
        let mut found = Vec::new();
        let mut done: Option<DateTime<FixedOffset>> = None;
        // The latest time read that is earlier than `done`: where a failure may resume.
        let mut below: Option<DateTime<FixedOffset>> = None;
        let mut read = 0;
        for (convo, last) in fresh {
            if resume.is_some_and(|r| last <= r) {
                continue;
            }
            // Never between two conversations of the same time: `resume` could not tell them apart.
            if read > 0
                && done.is_some_and(|d| last > d)
                && (read >= MAX_CONVOS || Instant::now() >= deadline)
            {
                return Ok(Messages {
                    found,
                    newest: None,
                    resume: done,
                    stopped: None,
                });
            }
            let Some(id) = convo["id"].as_str() else {
                continue;
            };
            let q = [("convoId", id), ("limit", "50")];
            let msgs = match self.xrpc(c, "chat.bsky.convo.getMessages", &q, None).await {
                Ok(m) => m,
                // The conversations read before this one keep their messages. `resume` must
                // sit below this one's time (it skips what is at or before it), so one that
                // shares the time of the last read is read again, which stores nothing twice.
                Err(e) => {
                    let resume = if done == Some(last) { below } else { done };
                    return match resume {
                        Some(_) => Ok(Messages {
                            found,
                            newest: None,
                            resume,
                            stopped: Some(e),
                        }),
                        None => Err(e),
                    };
                }
            };
            if done.is_some_and(|d| d < last) {
                below = done;
            }
            (done, read) = (Some(last), read + 1);
            let members = convo["members"].as_array().cloned().unwrap_or_default();
            // Newer conversations first, like the messages inside one.
            let mut batch = Vec::new();
            for m in msgs["messages"].as_array().into_iter().flatten() {
                if m["$type"] != "chat.bsky.convo.defs#messageView" {
                    continue;
                }
                let (Some(mid), Some(sent_at)) = (m["id"].as_str(), time(&m["sentAt"])) else {
                    continue;
                };
                let sender = m["sender"]["did"].as_str().unwrap_or_default();
                if sent_at <= since || sender == c.did || sender.is_empty() {
                    continue;
                }
                let profile = members.iter().find(|p| p["did"] == sender);
                batch.push(Message {
                    convo_id: id.to_owned(),
                    id: mid.to_owned(),
                    sender: profile.and_then(author).unwrap_or_else(|| Author {
                        did: sender.to_owned(),
                        ..Author::default()
                    }),
                    text: m["text"].as_str().unwrap_or_default().to_owned(),
                    sent_at,
                });
            }
            found.splice(0..0, batch);
        }
        Ok(Messages {
            found,
            // Without the requests, the mark stays where it was: the next round reads this
            // one's conversations again (storing nothing twice) and the requests with them.
            newest: newest.filter(|_| !requests_missed),
            resume: None,
            stopped: None,
        })
    }

    /// Replies to `parent` in the thread of `root`; returns the reply's `at://` URI, empty when
    /// Bluesky took it without saying which (it was still sent). `skip`: see `facets`.
    pub async fn reply(
        &self,
        c: &Creds<'_>,
        text: &str,
        skip: &[Range<usize>],
        parent: Ref<'_>,
        root: Ref<'_>,
    ) -> Result<String, InboxError> {
        let extra = self.rich_facets(text, skip).await;
        let record = post_record_with(text, skip, Some((parent, root)), extra);
        let body =
            serde_json::json!({ "repo": c.did, "collection": POST_COLLECTION, "record": record });
        let made = self
            .xrpc(c, "com.atproto.repo.createRecord", &[], Some(&body))
            .await?;
        let uri = made["uri"].as_str().filter(|u| post_key(u).is_some());
        Ok(uri.unwrap_or_default().to_owned())
    }

    /// Sends a DM in `convo`, or to `to` (a DID) when there is none yet; returns the message
    /// id, empty when Bluesky did not say (it was still sent). `dm_not_allowed` when the
    /// recipient does not take messages from the account. `skip`: see `facets`.
    pub async fn send_dm(
        &self,
        c: &Creds<'_>,
        convo: Option<&str>,
        to: &str,
        text: &str,
        skip: &[Range<usize>],
    ) -> Result<String, InboxError> {
        let convo = match convo {
            Some(id) => id.to_owned(),
            None => {
                let q = [("members", to)];
                let can = self
                    .xrpc(c, "chat.bsky.convo.getConvoAvailability", &q, None)
                    .await?;
                if can["canChat"] != true {
                    return Err(InboxError::DmNotAllowed);
                }
                match can["convo"]["id"].as_str() {
                    Some(id) => id.to_owned(),
                    None => {
                        let made = self
                            .xrpc(c, "chat.bsky.convo.getConvoForMembers", &q, None)
                            .await?;
                        made["convo"]["id"]
                            .as_str()
                            .ok_or(InboxError::Invalid("no_convo"))?
                            .to_owned()
                    }
                }
            }
        };
        let mut message = serde_json::json!({ "text": text });
        let facets = merged(facets(text, skip), self.rich_facets(text, skip).await);
        if !facets.is_empty() {
            message["facets"] = facets.into();
        }
        let body = serde_json::json!({ "convoId": convo, "message": message });
        let sent = self
            .xrpc(c, "chat.bsky.convo.sendMessage", &[], Some(&body))
            .await
            .map_err(|e| match e {
                InboxError::Refused { ref code, .. } if code == "ConvoLocked" => {
                    InboxError::Invalid("convo_locked")
                }
                // The chat service answers 400 when the recipient's settings refuse the sender.
                InboxError::Refused { status: 400, .. } => InboxError::DmNotAllowed,
                e => e,
            })?;
        Ok(sent["id"].as_str().unwrap_or_default().to_owned())
    }
}

/// Resolves names with the system resolver and keeps only public addresses, so a DID document
/// cannot point brain at its own network. reqwest connects to exactly these addresses, so a
/// rebinding name cannot swap in another afterwards. IP literals skip the resolver;
/// `checked_pds` refuses those.
pub(crate) struct PublicOnly;

pub(crate) const NOT_PUBLIC: &str = "name has no public address";

#[derive(Debug)]
struct NotPublic;

impl std::fmt::Display for NotPublic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(NOT_PUBLIC)
    }
}

impl std::error::Error for NotPublic {}

fn public_only(addrs: impl Iterator<Item = SocketAddr>) -> Result<Vec<SocketAddr>, NotPublic> {
    let kept: Vec<SocketAddr> = addrs.filter(|a| public(a.ip())).collect();
    if kept.is_empty() {
        Err(NotPublic)
    } else {
        Ok(kept)
    }
}

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let found = tokio::net::lookup_host((name.as_str(), 0)).await?;
            let kept = public_only(found)?;
            Ok(Box::new(kept.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Not loopback, private, link-local, carrier-grade NAT, multicast or otherwise reserved.
pub(crate) fn public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || a == 0
                || a >= 240
                || (a == 100 && (64..128).contains(&b)))
        }
        IpAddr::V6(v6) => match embedded_v4(v6) {
            Some(v4) => public(IpAddr::V4(v4)),
            None => {
                let [a, b, c, d, ..] = v6.segments();
                !(v6.to_bits() >> 32 == 0 // ::/128, ::1 and IPv4-compatible ::/96
                    || v6.is_multicast()
                    || (a & 0xfe00) == 0xfc00 // ULA
                    || (a & 0xffc0) == 0xfe80 // link-local
                    || (a == 0x2001 && b == 0x0db8) // documentation
                    || (a == 0x2001 && b == 0) // Teredo
                    || (a == 0x2001 && (b & 0xfff0) == 0x0010) // ORCHID
                    || (a == 0x2001 && (b & 0xfff0) == 0x0020) // ORCHIDv2
                    || (a == 0x0100 && b == 0 && c == 0 && d == 0)) // discard
            }
        },
    }
}

/// The IPv4 address inside an IPv4-mapped (`::ffff:0:0/96`), NAT64 (`64:ff9b::/96`, local-use
/// `64:ff9b:1::/48`) or 6to4 (`2002::/16`) address, which is where a connection really goes.
fn embedded_v4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = v6.segments();
    let pair = |hi: u16, lo: u16| Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo));
    let [o0, o1] = s[3].to_be_bytes();
    match s {
        _ if v6.to_ipv4_mapped().is_some() => v6.to_ipv4_mapped(),
        [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] => Some(pair(hi, lo)),
        // RFC 6052 /48 layout: octets in bits 48..64 and 72..88, skipping the u octet.
        [0x64, 0xff9b, 0x0001, ..] => Some(Ipv4Addr::new(
            o0,
            o1,
            s[4].to_be_bytes()[1],
            s[5].to_be_bytes()[0],
        )),
        [0x2002, hi, lo, ..] => Some(pair(hi, lo)),
        _ => None,
    }
}

/// Link facets for `text`, as posts and chat messages take them, leaving out any link that
/// touches a byte range in `skip` (text filled in from someone else, e.g. a commenter's name),
/// so nobody can make us send a live link.
fn facets(text: &str, skip: &[Range<usize>]) -> Vec<Value> {
    socialreins_core::post::links(text)
        .into_iter()
        .filter(|l| !skip.iter().any(|r| l.start < r.end && r.start < l.end))
        .map(|l| {
            serde_json::json!({
                "index": { "byteStart": l.start, "byteEnd": l.end },
                "features": [{ "$type": "app.bsky.richtext.facet#link", "uri": l.url }],
            })
        })
        .collect()
}

/// `@handle` tokens in `text` that could be mentions: after the start, whitespace or an opening
/// bracket, a domain name of letters, digits, dots and hyphens with a dot in it, at most 5, none
/// touching a range in `skip` (text filled in from someone else). Byte range (with the `@`) and
/// the normalized handle.
fn mention_candidates(text: &str, skip: &[Range<usize>]) -> Vec<(Range<usize>, String)> {
    let mut found = Vec::new();
    for (at, _) in text.match_indices('@') {
        let before = text[..at].chars().next_back();
        if before.is_some_and(|c| !c.is_whitespace() && !"([{".contains(c)) {
            continue;
        }
        let tail = &text[at + 1..];
        let len = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
            .unwrap_or(tail.len());
        let name = tail[..len].trim_end_matches(['.', '-']);
        let range = at..at + 1 + name.len();
        if skip
            .iter()
            .any(|r| range.start < r.end && r.start < range.end)
        {
            continue;
        }
        if let Some(h) = handle(name) {
            found.push((range, h));
        }
        if found.len() == 5 {
            break;
        }
    }
    found
}

/// `#tag` facets for `text`, outside `skip`: 1 to 64 characters of letters, digits and `_`, not
/// only digits, after the start, whitespace or an opening bracket.
fn tag_facets(text: &str, skip: &[Range<usize>]) -> Vec<Value> {
    let mut out = Vec::new();
    for (at, _) in text.match_indices('#') {
        let before = text[..at].chars().next_back();
        if before.is_some_and(|c| !c.is_whitespace() && !"([{".contains(c)) {
            continue;
        }
        let tail = &text[at + 1..];
        let len = tail
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(tail.len());
        let name = &tail[..len];
        let range = at..at + 1 + name.len();
        let overlaps = skip
            .iter()
            .any(|r| range.start < r.end && r.start < range.end);
        if name.is_empty()
            || name.chars().count() > 64
            || name.chars().all(|c| c.is_ascii_digit())
            || overlaps
        {
            continue;
        }
        out.push(serde_json::json!({
            "index": { "byteStart": range.start, "byteEnd": range.end },
            "features": [{ "$type": "app.bsky.richtext.facet#tag", "tag": name }],
        }));
    }
    out
}

/// `links` and `extra` facets in text order, dropping any that overlaps one already kept.
fn merged(links: Vec<Value>, extra: Vec<Value>) -> Vec<Value> {
    let start = |f: &Value| f["index"]["byteStart"].as_u64().unwrap_or(0);
    let end = |f: &Value| f["index"]["byteEnd"].as_u64().unwrap_or(0);
    let mut all: Vec<Value> = links.into_iter().chain(extra).collect();
    all.sort_by_key(start);
    let mut kept: Vec<Value> = Vec::new();
    for f in all {
        if kept.last().is_none_or(|k| start(&f) >= end(k)) {
            kept.push(f);
        }
    }
    kept
}

impl Bluesky {
    /// Mention and tag facets for `text`: each `@handle` that Bluesky resolves becomes a mention
    /// of its DID (one that does not resolve stays plain text), each `#tag` a tag.
    async fn rich_facets(&self, text: &str, skip: &[Range<usize>]) -> Vec<Value> {
        let mut out = tag_facets(text, skip);
        for (range, handle) in mention_candidates(text, skip) {
            if let Ok(did) = self.did(&handle).await {
                out.push(serde_json::json!({
                    "index": { "byteStart": range.start, "byteEnd": range.end },
                    "features": [{ "$type": "app.bsky.richtext.facet#mention", "did": did }],
                }));
            }
        }
        out
    }
}

/// A post record, a reply when `reply` holds `(parent, root)` as `(uri, cid)` pairs.
pub fn post_record(text: &str, skip: &[Range<usize>], reply: Option<(Ref<'_>, Ref<'_>)>) -> Value {
    post_record_with(text, skip, reply, Vec::new())
}

/// `post_record` with `extra` facets (mentions, tags) beside the links.
pub fn post_record_with(
    text: &str,
    skip: &[Range<usize>],
    reply: Option<(Ref<'_>, Ref<'_>)>,
    extra: Vec<Value>,
) -> Value {
    let created = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut record = serde_json::json!({
        "$type": POST_COLLECTION, "text": text, "createdAt": created,
    });
    let facets = merged(facets(text, skip), extra);
    if !facets.is_empty() {
        record["facets"] = facets.into();
    }
    if let Some((parent, root)) = reply {
        record["reply"] = serde_json::json!({
            "root": { "uri": root.0, "cid": root.1 },
            "parent": { "uri": parent.0, "cid": parent.1 },
        });
    }
    record
}

/// A post's `(at:// uri, cid)`.
pub type Ref<'a> = (&'a str, &'a str);

/// The access token's claims; `None` when unreadable.
fn claims(jwt: &str) -> Option<Value> {
    jwt.split('.')
        .nth(1)
        .and_then(|p| URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
}

/// An app password made with "Allow access to your direct messages".
fn privileged(jwt: &str) -> bool {
    claims(jwt).is_some_and(|c| c["scope"] == "com.atproto.appPassPrivileged")
}

/// The session's access token carries the account password's full scope
/// (`com.atproto.access`); an app password gets `com.atproto.appPass` or
/// `com.atproto.appPassPrivileged`. An unreadable token is not treated as full access.
fn full_access(jwt: &str) -> bool {
    claims(jwt).is_some_and(|c| c["scope"] == "com.atproto.access")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_urls_come_from_at_uris() {
        let uri = "at://did:plc:abc123/app.bsky.feed.post/3kxyz";
        assert_eq!(
            post_url(uri).as_deref(),
            Some("https://bsky.app/profile/did:plc:abc123/post/3kxyz")
        );
        for bad in [
            "at://did:plc:abc/app.bsky.feed.like/3k",
            "at://did:plc:abc/app.bsky.feed.post/",
            "at://did:plc:abc/app.bsky.feed.post/3k/x",
            "https://bsky.app/x",
            "at://did:plc:a b/app.bsky.feed.post/3k",
        ] {
            assert_eq!(post_url(bad), None, "{bad}");
        }
    }

    fn mp4(width: u32, height: u32, matrix_a: u32) -> Vec<u8> {
        let mut tkhd = vec![0u8; 84];
        tkhd[40..44].copy_from_slice(&matrix_a.to_be_bytes());
        tkhd[76..80].copy_from_slice(&(width << 16).to_be_bytes());
        tkhd[80..84].copy_from_slice(&(height << 16).to_be_bytes());
        let wrap = |name: &[u8; 4], body: Vec<u8>| {
            let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            b.extend_from_slice(name);
            b.extend(body);
            b
        };
        let mut file = wrap(b"ftyp", b"mp42\0\0\0\0".to_vec());
        file.extend(wrap(b"moov", wrap(b"trak", wrap(b"tkhd", tkhd))));
        file
    }

    #[tokio::test]
    async fn the_moov_box_is_found_without_reading_the_video_data() {
        // `ftyp`, 3 MB of video data, then `moov`, as a phone writes it: in memory or stored.
        let small = mp4(1080, 1920, 0x10000);
        let (ftyp, tail) = small.split_at(16);
        let mut mdat = (3_000_008u32).to_be_bytes().to_vec();
        mdat.extend_from_slice(b"mdat");
        mdat.resize(3_000_008, 7);
        let file = [ftyp, &mdat, tail].concat();
        let store = Arc::new(object_store::memory::InMemory::new());
        let path = object_store::path::Path::from("posts/p/v");
        object_store::ObjectStoreExt::put(&*store, &path, file.clone().into())
            .await
            .unwrap();
        let stored = Blob::Stored {
            store,
            key: "posts/p/v".into(),
            size: file.len() as u64,
        };
        for video in [Blob::Held(file.clone().into()), stored] {
            let moov = moov(&video).await.unwrap();
            assert_eq!(&moov[..], tail);
            assert_eq!(mp4_size(&moov), Some((1080, 1920)));
        }
        assert!(moov(&Blob::Held(mdat.into())).await.is_none());
        assert!(moov(&Blob::default()).await.is_none());
        assert!(moov(&Blob::Held(vec![0u8; 64].into())).await.is_none());
    }

    #[test]
    fn mp4_size_reads_the_track_header() {
        assert_eq!(mp4_size(&mp4(1080, 1920, 0x10000)), Some((1080, 1920)));
        assert_eq!(mp4_size(&mp4(1920, 1080, 0)), Some((1080, 1920)), "turned");
        assert_eq!(mp4_size(&mp4(0, 0, 0x10000)), None);
        assert_eq!(mp4_size(b"not a video"), None);
        let cut = mp4(1080, 1920, 0x10000);
        assert_eq!(mp4_size(&cut[..cut.len() - 10]), None);
    }

    #[test]
    fn mentions_and_tags_are_found_where_bluesky_would() {
        let text =
            "Thanks @Alice.test, (@bob.example.com) and mail me@x.org #launch_day #2026 a#b @nodot";
        let names: Vec<_> = mention_candidates(text, &[])
            .into_iter()
            .map(|(r, h)| (text[r].to_owned(), h))
            .collect();
        assert_eq!(
            names,
            [
                ("@Alice.test".to_owned(), "alice.test".to_owned()),
                ("@bob.example.com".to_owned(), "bob.example.com".to_owned())
            ]
        );
        let tags: Vec<_> = tag_facets(text, &[])
            .iter()
            .map(|f| f["features"][0]["tag"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            tags,
            ["launch_day"],
            "digits only and mid-word are not tags"
        );
        // Text filled in from someone else never becomes a mention or a tag.
        let r = text.find("@Alice").unwrap();
        assert!(mention_candidates(text, std::slice::from_ref(&(r..r + 11)))
            .iter()
            .all(|(_, h)| h != "alice.test"));
        assert!(tag_facets("#x", std::slice::from_ref(&(0..2))).is_empty());
        // At most 5 mentions.
        let many = "@a.test @b.test @c.test @d.test @e.test @f.test";
        assert_eq!(mention_candidates(many, &[]).len(), 5);
    }

    #[test]
    fn facets_come_in_text_order_without_overlaps() {
        let f = |a: u64, b: u64, t: &str| serde_json::json!({"index": {"byteStart": a, "byteEnd": b}, "features": [{"$type": t}]});
        let got = merged(
            vec![f(10, 20, "link")],
            vec![f(0, 5, "tag"), f(12, 15, "mention"), f(20, 25, "tag")],
        );
        let kinds: Vec<_> = got
            .iter()
            .map(|x| x["features"][0]["$type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["tag", "link", "tag"]);
    }

    #[test]
    fn xrpc_codes_are_scrubbed() {
        let s = StatusCode::BAD_REQUEST;
        assert_eq!(
            xrpc_code(&serde_json::json!({"error": "InvalidRequest"}), s),
            "InvalidRequest"
        );
        let leaky = serde_json::json!({"error": "see https://x.io?token=abc"});
        assert_eq!(xrpc_code(&leaky, s), "HTTP 400");
        assert_eq!(xrpc_code(&Value::Null, s), "HTTP 400");
    }

    #[test]
    fn handles_are_normalized_domains() {
        assert_eq!(
            handle(" @Alice.BSKY.social ").as_deref(),
            Some("alice.bsky.social")
        );
        assert_eq!(handle("me.example.com").as_deref(), Some("me.example.com"));
        for bad in [
            "", "alice", "a..b", "-a.com", "a b.com", "a/b.com", "a.com:80",
        ] {
            assert_eq!(handle(bad), None, "{bad}");
        }
    }

    #[test]
    fn pds_must_be_public_https() {
        let b = Bluesky::new("https://appview.test", "https://plc.test").unwrap();
        assert_eq!(
            b.checked_pds("https://pds.example.com/").unwrap(),
            "https://pds.example.com"
        );
        for bad in [
            "http://pds.example.com",
            "https://127.0.0.1",
            "https://[::1]",
            "https://localhost",
            "https://user:pw@pds.example.com",
            "https://pds.example.com/?x=1",
            "ftp://pds.example.com",
            "not a url",
        ] {
            assert_eq!(b.checked_pds(bad), Err(Error::UnsupportedPds), "{bad}");
        }
    }

    #[test]
    fn only_public_addresses_resolve() {
        for ok in ["8.8.8.8", "2606:4700::1111"] {
            assert!(public(ok.parse().unwrap()), "{ok}");
        }
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "2001:db8::1",
            "ff02::1",
            "192.0.2.1",
            "224.0.0.1",
        ] {
            assert!(!public(bad.parse().unwrap()), "{bad}");
        }
    }

    #[test]
    fn ipv6_special_ranges_are_judged_by_what_they_reach() {
        let table = [
            ("2606:4700::1111", true),
            ("::ffff:8.8.8.8", true),
            ("::ffff:10.0.0.1", false),
            ("64:ff9b::808:808", true),
            ("64:ff9b::a00:1", false),
            ("64:ff9b::7f00:1", false),
            ("64:ff9b:1:808:8:800::", true),
            ("64:ff9b:1:a00:0:100::", false),
            ("2002:808:808::1", true),
            ("2002:a9fe:a9fe::1", false),
            ("2002:7f00:1::1", false),
            ("2001::1", false),
            ("2001:0:4136:e378::1", false),
            ("100::1", false),
            ("2001:10::1", false),
            ("2001:1f::1", false),
            ("2001:20::1", false),
            ("2001:2f::1", false),
            ("2001:db8::1", false),
            ("::", false),
            ("::1", false),
            ("::8.8.8.8", false),
            ("fc00::1", false),
            ("fe80::1", false),
            ("ff02::1", false),
        ];
        for (ip, want) in table {
            assert_eq!(public(ip.parse().unwrap()), want, "{ip}");
        }
    }

    #[test]
    fn the_resolver_keeps_public_addresses_or_fails() {
        let addr = |s: &str| SocketAddr::new(s.parse().unwrap(), 0);
        let mixed = [addr("10.0.0.1"), addr("8.8.8.8"), addr("::1")];
        assert_eq!(public_only(mixed.into_iter()).unwrap(), [addr("8.8.8.8")]);
        let private = [addr("127.0.0.1"), addr("169.254.169.254")];
        assert!(public_only(private.into_iter()).is_err());
        assert!(public_only(std::iter::empty()).is_err());
    }

    #[test]
    fn notifications_parse_into_comments_and_mentions() {
        let n = serde_json::json!({
            "uri": "at://did:plc:fan/app.bsky.feed.post/r1", "cid": "c1", "reason": "reply",
            "author": {"did": "did:plc:fan", "handle": "fan.test", "displayName": "Fan"},
            "record": {"text": "hi", "reply": {
                "root": {"uri": "at://did:plc:me/app.bsky.feed.post/p0", "cid": "c0"},
                "parent": {"uri": "at://did:plc:me/app.bsky.feed.post/p1", "cid": "cp"}}},
            "indexedAt": "2026-10-30T10:00:00.000Z",
        });
        let got = notification(&n).unwrap();
        assert_eq!(got.kind, "comment");
        assert_eq!(got.author.display_name.as_deref(), Some("Fan"));
        assert_eq!(
            got.parent_uri.as_deref(),
            Some("at://did:plc:me/app.bsky.feed.post/p1")
        );
        assert_eq!(got.root.0, "at://did:plc:me/app.bsky.feed.post/p0");
        let mut quote = n.clone();
        quote["reason"] = "quote".into();
        quote["record"] = serde_json::json!({"text": "look"});
        let got = notification(&quote).unwrap();
        assert_eq!(got.kind, "mention");
        assert_eq!(
            got.root,
            (
                "at://did:plc:fan/app.bsky.feed.post/r1".into(),
                Some("c1".into())
            )
        );
        let follow = serde_json::json!({
            "uri": "at://did:plc:fan/app.bsky.graph.follow/f1", "cid": "cf", "reason": "follow",
            "author": {"did": "did:plc:fan", "handle": "fan.test"},
            "record": {"$type": "app.bsky.graph.follow", "subject": "did:plc:me"},
            "indexedAt": "2026-10-30T10:00:00.000Z",
        });
        let got = notification(&follow).unwrap();
        assert_eq!((got.kind, got.text.as_str()), ("follow", ""));
        assert_eq!(got.author.did, "did:plc:fan");
        let mut like = n;
        like["reason"] = "like".into();
        assert_eq!(notification(&like), None);
    }

    #[test]
    fn notifications_keep_what_the_post_embeds() {
        let n = |embed: Value| {
            let n = serde_json::json!({
                "uri": "at://did:plc:fan/app.bsky.feed.post/r1", "cid": "c1", "reason": "mention",
                "author": {"did": "did:plc:fan"}, "record": {"text": "", "embed": embed},
                "indexedAt": "2026-10-30T10:00:00.000Z",
            });
            notification(&n).unwrap().attachments
        };
        let blob = |cid: &str| serde_json::json!({"$type": "blob", "ref": {"$link": cid}});
        let images = n(serde_json::json!({"$type": "app.bsky.embed.images",
            "images": [{"alt": "", "image": blob("bafk1")}, {"image": blob("../x")}]}));
        assert_eq!(
            images,
            [Attachment {
                kind: "image".into(),
                url: "https://cdn.bsky.app/img/feed_fullsize/plain/did:plc:fan/bafk1@jpeg".into(),
                preview_url: Some(
                    "https://cdn.bsky.app/img/feed_thumbnail/plain/did:plc:fan/bafk1@jpeg".into()
                ),
            }]
        );
        let video = n(
            serde_json::json!({"$type": "app.bsky.embed.recordWithMedia",
            "record": {}, "media": {"$type": "app.bsky.embed.video", "video": blob("bafv")}}),
        );
        // The stream does not play in a tab: the video opens its post, its still is the preview.
        assert_eq!(video[0].kind, "video");
        assert_eq!(video[0].url, "https://bsky.app/profile/did:plc:fan/post/r1");
        assert_eq!(
            video[0].preview_url.as_deref(),
            Some("https://video.bsky.app/watch/did:plc:fan/bafv/thumbnail.jpg")
        );
        let link = n(serde_json::json!({"$type": "app.bsky.embed.external",
            "external": {"uri": "https://example.com/a", "title": "A"}}));
        assert_eq!(
            (link[0].kind.as_str(), link[0].url.as_str()),
            ("link", "https://example.com/a")
        );
        assert_eq!(link[0].preview_url, None);
        let unsafe_link = n(serde_json::json!({"$type": "app.bsky.embed.external",
            "external": {"uri": "javascript:alert(1)"}}));
        assert!(unsafe_link.is_empty());
        assert!(n(Value::Null).is_empty());
    }

    #[test]
    fn reply_records_point_at_parent_and_root() {
        let r = post_record(
            "thanks https://x.io",
            &[],
            Some((("at://p", "cp"), ("at://r", "cr"))),
        );
        assert_eq!(r["$type"], POST_COLLECTION);
        assert_eq!(
            r["reply"]["parent"],
            serde_json::json!({"uri": "at://p", "cid": "cp"})
        );
        assert_eq!(
            r["reply"]["root"],
            serde_json::json!({"uri": "at://r", "cid": "cr"})
        );
        assert_eq!(r["facets"][0]["features"][0]["uri"], "https://x.io");
        assert!(post_record("plain", &[], None).get("reply").is_none());
        // A link touching filled-in text gets no facet; the template's own link keeps one.
        let text = "Hi https://evil.example see https://x.io";
        let r = post_record(text, std::slice::from_ref(&(3..23)), None);
        assert_eq!(r["facets"].as_array().unwrap().len(), 1);
        assert_eq!(r["facets"][0]["features"][0]["uri"], "https://x.io");
    }

    #[test]
    fn only_the_full_access_scope_counts_as_an_account_password() {
        let jwt = |scope: &str| {
            let claims = serde_json::json!({ "scope": scope }).to_string();
            format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims))
        };
        assert!(full_access(&jwt("com.atproto.access")));
        assert!(!full_access(&jwt("com.atproto.appPass")));
        assert!(!full_access(&jwt("com.atproto.appPassPrivileged")));
        assert!(!full_access("garbage"));
        assert!(privileged(&jwt("com.atproto.appPassPrivileged")));
        assert!(!privileged(&jwt("com.atproto.appPass")));
    }
}
