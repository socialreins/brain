//! Bluesky: no OAuth. A handle and an app password are checked by resolving the handle to a
//! DID, the DID to its PDS, and opening a session on that PDS. The app password only ever goes
//! to the resolved PDS, over https, with redirects off.
use std::{
    net::{IpAddr, SocketAddr},
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
    unreachable(e, what)
}

/// The JSON body, read up to `MAX_BODY` bytes.
async fn json_body<T: DeserializeOwned>(
    mut res: reqwest::Response,
    what: &str,
) -> Result<T, Error> {
    let too_large = || unreachable("body over 64 KiB", what);
    if res.content_length().is_some_and(|n| n > MAX_BODY as u64) {
        return Err(too_large());
    }
    let mut buf = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(|e| unreachable(e, what))? {
        if buf.len() + chunk.len() > MAX_BODY {
            return Err(too_large());
        }
        buf.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&buf).map_err(|e| unreachable(e, what))
}

#[derive(Deserialize)]
struct Resolved {
    did: String,
}

#[derive(Deserialize)]
struct DidDoc {
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

    /// Checks `app_password` on the account `handle` (already normalized by [`handle`]).
    pub async fn verify(&self, handle: &str, app_password: &str) -> Result<Account, Error> {
        let did = self.resolve(handle).await?;
        let pds = self.pds(&did).await?;
        self.session(&pds, &did, app_password).await?;
        let profile = self.profile(&did).await;
        Ok(Account {
            did,
            // The handle the AppView resolved to this DID; the PDS's own claim is not trusted.
            handle: handle.to_owned(),
            display_name: profile.as_ref().and_then(|p| p.display_name.clone()),
            avatar: profile.and_then(|p| p.avatar),
            pds,
        })
    }

    async fn resolve(&self, handle: &str) -> Result<String, Error> {
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
        let did = json_body::<Resolved>(res, "resolveHandle body").await?.did;
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
    async fn pds(&self, did: &str) -> Result<String, Error> {
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

    async fn session(&self, pds: &str, did: &str, password: &str) -> Result<(), Error> {
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
        if full_access(&session.access_jwt) {
            return Err(Error::InvalidCredentials);
        }
        Ok(())
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
        // IPv4-mapped and IPv4-compatible addresses are judged as the IPv4 address they hold.
        IpAddr::V6(v6) => match v6.to_ipv4() {
            Some(v4) => public(IpAddr::V4(v4)),
            None => {
                let [first, second, ..] = v6.segments();
                !(v6.is_multicast()
                    || (first & 0xfe00) == 0xfc00
                    || (first & 0xffc0) == 0xfe80
                    || (first == 0x2001 && second == 0x0db8))
            }
        },
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
