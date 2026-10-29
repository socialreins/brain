//! Bluesky: no OAuth. A handle and an app password are checked by resolving the handle to a
//! DID, the DID to its PDS, and opening a session on that PDS. The app password only ever goes
//! to the resolved PDS, over https, with redirects off.
use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::{StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::Value;

/// Where handles are resolved and profiles read, and where `did:plc` documents live.
#[derive(Clone)]
pub struct Bluesky {
    pub appview: String,
    pub plc: String,
    /// Tests only, never read from the environment: lets the PDS be a plain `http://127.0.0.1`
    /// mock. Names still resolve to public addresses only.
    pub allow_insecure_pds: bool,
    client: reqwest::Client,
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
    let buf = match read_capped(res, MAX_BODY).await {
        Ok(buf) => buf,
        Err(Read::TooLarge) => return Err(unreachable("body over 64 KiB", what)),
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
            allow_insecure_pds: false,
            client,
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
        Ok(Account {
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

/// Bluesky's limit for one image blob.
pub const MAX_IMAGE_BYTES: usize = 1_000_000;
const IMAGE_TYPES: [&str; 4] = ["image/jpeg", "image/png", "image/webp", "image/gif"];
const POST_COLLECTION: &str = "app.bsky.feed.post";

/// Why a publish or takedown did not happen. Messages are safe to store and show: they never
/// hold a URL, token or secret.
#[derive(Debug, PartialEq, Eq)]
pub enum PostError {
    /// The app password was refused: the account must be connected again.
    Reconnect,
    Failed(String),
}

fn failed(msg: impl Into<String>) -> PostError {
    PostError::Failed(msg.into())
}

/// An XRPC error name (`InvalidRequest`), kept only when it is a plain word, else the status.
fn xrpc_code(body: &Value, status: StatusCode) -> String {
    match body["error"].as_str() {
        Some(e)
            if !e.is_empty() && e.len() <= 40 && e.chars().all(|c| c.is_ascii_alphanumeric()) =>
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

impl Bluesky {
    /// A session on the account's PDS (`meta.pds`, checked again) with its app password.
    async fn login(
        &self,
        pds: &str,
        did: &str,
        password: &str,
    ) -> Result<(String, String), PostError> {
        let pds = self
            .checked_pds(pds)
            .map_err(|_| failed("The account's Bluesky server is not supported."))?;
        match self.session(&pds, did, password).await {
            Ok(jwt) => Ok((pds, jwt)),
            Err(Error::InvalidCredentials) => Err(PostError::Reconnect),
            Err(Error::RateLimited) => Err(failed("Bluesky is rate limiting this account.")),
            Err(Error::UnsupportedPds) => {
                Err(failed("The account's Bluesky server is not supported."))
            }
            Err(_) => Err(failed("Bluesky could not be reached.")),
        }
    }

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
                tracing::warn!(error = %e.without_url(), method, "bluesky call failed");
                failed("Bluesky could not be reached.")
            })?;
        let status = res.status();
        let answer: Value = json_body(res, method).await.unwrap_or_default();
        match status {
            StatusCode::OK => Ok(answer),
            StatusCode::UNAUTHORIZED => Err(PostError::Reconnect),
            StatusCode::TOO_MANY_REQUESTS => Err(failed("Bluesky is rate limiting this account.")),
            s => Err(failed(format!(
                "Bluesky refused it ({}).",
                xrpc_code(&answer, s)
            ))),
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

    /// Downloads image `n` (1-based, for messages) of a post: https only, a public address,
    /// an image type Bluesky takes, at most 1 MB.
    async fn image(&self, n: usize, url: &str) -> Result<(Vec<u8>, String), PostError> {
        let bad = || failed(format!("Image {n} is not a public https URL."));
        let parsed = Url::parse(url).map_err(|_| bad())?;
        let host = parsed.host_str().unwrap_or_default();
        let named = !host.is_empty() && host.trim_matches(['[', ']']).parse::<IpAddr>().is_err();
        let secure = parsed.scheme() == "https" && named;
        if !parsed.username().is_empty() || !(secure || self.allow_insecure_pds) {
            return Err(bad());
        }
        let res = self.client.get(parsed).send().await.map_err(|e| {
            tracing::warn!(error = %e.without_url(), n, "image download failed");
            failed(format!("Image {n} could not be downloaded."))
        })?;
        if res.status() != StatusCode::OK {
            return Err(failed(format!(
                "Image {n} could not be downloaded (HTTP {}).",
                res.status().as_u16()
            )));
        }
        let mime = res
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| {
                v.split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase()
            })
            .unwrap_or_default();
        if !IMAGE_TYPES.contains(&mime.as_str()) {
            return Err(failed(format!(
                "Image {n} is not a JPEG, PNG, WebP or GIF."
            )));
        }
        match read_capped(res, MAX_IMAGE_BYTES).await {
            Ok(bytes) => Ok((bytes, mime)),
            Err(Read::TooLarge) => Err(failed(format!("Image {n} is over 1 MB, Bluesky's limit."))),
            Err(Read::Failed(e)) => {
                tracing::warn!(error = %e.without_url(), n, "image download failed");
                Err(failed(format!("Image {n} could not be downloaded.")))
            }
        }
    }

    /// Publishes a post with up to 4 images; returns its `at://` URI.
    pub async fn publish_post(
        &self,
        pds: &str,
        did: &str,
        password: &str,
        text: &str,
        media: &[String],
    ) -> Result<String, PostError> {
        let (pds, jwt) = self.login(pds, did, password).await?;
        let mut images = Vec::new();
        for (i, url) in media.iter().enumerate() {
            let (bytes, mime) = self.image(i + 1, url).await?;
            let up = self
                .call(
                    &pds,
                    &jwt,
                    "com.atproto.repo.uploadBlob",
                    bytes.into(),
                    &mime,
                )
                .await?;
            if !up["blob"].is_object() {
                return Err(failed("Bluesky did not take an image."));
            }
            images.push(serde_json::json!({ "alt": "", "image": up["blob"] }));
        }
        let facets: Vec<Value> = socialreins_core::post::links(text)
            .into_iter()
            .map(|l| {
                serde_json::json!({
                    "index": { "byteStart": l.start, "byteEnd": l.end },
                    "features": [{ "$type": "app.bsky.richtext.facet#link", "uri": l.url }],
                })
            })
            .collect();
        let created = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut record = serde_json::json!({
            "$type": POST_COLLECTION, "text": text, "createdAt": created,
        });
        if !facets.is_empty() {
            record["facets"] = facets.into();
        }
        if !images.is_empty() {
            record["embed"] =
                serde_json::json!({ "$type": "app.bsky.embed.images", "images": images });
        }
        let body =
            serde_json::json!({ "repo": did, "collection": POST_COLLECTION, "record": record });
        let made = self
            .call_json(&pds, &jwt, "com.atproto.repo.createRecord", &body)
            .await?;
        match made["uri"].as_str() {
            Some(uri) if post_key(uri).is_some() => Ok(uri.to_owned()),
            _ => Err(failed("Bluesky did not return the post's id.")),
        }
    }

    /// Deletes the post at `uri` from the account's repo.
    pub async fn delete_post(
        &self,
        pds: &str,
        did: &str,
        password: &str,
        uri: &str,
    ) -> Result<(), PostError> {
        let Some((repo, rkey)) = post_key(uri).filter(|(repo, _)| *repo == did) else {
            return Err(failed("This post does not belong to the account."));
        };
        let (pds, jwt) = self.login(pds, did, password).await?;
        let body = serde_json::json!({ "repo": repo, "collection": POST_COLLECTION, "rkey": rkey });
        self.call_json(&pds, &jwt, "com.atproto.repo.deleteRecord", &body)
            .await
            .map(|_| ())
    }
}

/// Resolves names with the system resolver and keeps only public addresses, so a DID document
/// cannot point brain at its own network. reqwest connects to exactly these addresses, so a
/// rebinding name cannot swap in another afterwards. IP literals skip the resolver;
/// `checked_pds` refuses those.
struct PublicOnly;

const NOT_PUBLIC: &str = "name has no public address";

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
fn public(ip: IpAddr) -> bool {
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

/// The session's access token carries the account password's full scope
/// (`com.atproto.access`); an app password gets `com.atproto.appPass` or
/// `com.atproto.appPassPrivileged`. An unreadable token is not treated as full access.
fn full_access(jwt: &str) -> bool {
    let claims = jwt
        .split('.')
        .nth(1)
        .and_then(|p| URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    claims.is_some_and(|c| c["scope"] == "com.atproto.access")
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
    fn only_the_full_access_scope_counts_as_an_account_password() {
        let jwt = |scope: &str| {
            let claims = serde_json::json!({ "scope": scope }).to_string();
            format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims))
        };
        assert!(full_access(&jwt("com.atproto.access")));
        assert!(!full_access(&jwt("com.atproto.appPass")));
        assert!(!full_access(&jwt("com.atproto.appPassPrivileged")));
        assert!(!full_access("garbage"));
    }
}
