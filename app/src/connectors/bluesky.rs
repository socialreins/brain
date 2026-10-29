//! Bluesky: no OAuth. A handle and an app password are checked by resolving the handle to a
//! DID, the DID to its PDS, and opening a session on that PDS. The app password only ever goes
//! to the resolved PDS, over https, with redirects off.
use std::{net::IpAddr, time::Duration};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde_json::Value;

/// Where handles are resolved and profiles read, and where `did:plc` documents live.
#[derive(Clone)]
pub struct Bluesky {
    pub appview: String,
    pub plc: String,
    /// Tests only: lets the PDS be a plain `http://127.0.0.1` mock.
    pub allow_insecure_pds: bool,
    client: reqwest::Client,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    InvalidHandle,
    /// The PDS is not a public https URL.
    UnsupportedPds,
    InvalidCredentials,
    /// The password opened a full session: it is the account password, not an app password.
    AppPasswordRequired,
    RateLimited,
    Unreachable,
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidHandle => "invalid_handle",
            Self::UnsupportedPds => "unsupported_pds",
            Self::InvalidCredentials => "invalid_credentials",
            Self::AppPasswordRequired => "app_password_required",
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

fn unreachable(e: impl std::fmt::Display, what: &str) -> Error {
    tracing::warn!(error = %e, what, "bluesky call failed");
    Error::Unreachable
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
    handle: String,
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
        let session = self.session(&pds, &did, app_password).await?;
        let profile = self.profile(&did).await;
        Ok(Account {
            did,
            handle: session.handle,
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
            .map_err(|e| unreachable(e, "resolveHandle"))?;
        match res.status() {
            StatusCode::OK => {}
            StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND => return Err(Error::InvalidHandle),
            s => return Err(unreachable(s, "resolveHandle")),
        }
        let did = res
            .json::<Resolved>()
            .await
            .map_err(|e| unreachable(e, "resolveHandle body"))?
            .did;
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
            .map_err(|e| unreachable(e, "did document"))?;
        match res.status() {
            StatusCode::OK => {}
            StatusCode::NOT_FOUND | StatusCode::GONE => return Err(Error::InvalidHandle),
            s => return Err(unreachable(s, "did document")),
        }
        let doc = res
            .json::<DidDoc>()
            .await
            .map_err(|e| unreachable(e, "did document body"))?;
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

    async fn session(&self, pds: &str, did: &str, password: &str) -> Result<Session, Error> {
        let url = format!("{pds}/xrpc/com.atproto.server.createSession");
        let body = serde_json::json!({"identifier": did, "password": password});
        let res = self
            .client
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| unreachable(e, "createSession"))?;
        let status = res.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(Error::RateLimited);
        }
        if status == StatusCode::BAD_REQUEST || status == StatusCode::UNAUTHORIZED {
            // Only an account password with email 2FA asks for a second factor.
            let err = res.json::<Value>().await.unwrap_or_default();
            return Err(match err["error"].as_str() {
                Some("AuthFactorTokenRequired") => Error::AppPasswordRequired,
                _ => Error::InvalidCredentials,
            });
        }
        if status != StatusCode::OK {
            return Err(unreachable(status, "createSession"));
        }
        let session = res
            .json::<Session>()
            .await
            .map_err(|e| unreachable(e, "createSession body"))?;
        if session.did != did {
            return Err(Error::InvalidCredentials);
        }
        if full_access(&session.access_jwt) {
            return Err(Error::AppPasswordRequired);
        }
        Ok(session)
    }

    /// Display name and avatar from the public AppView; `None` when it has none to give, which
    /// does not stop the connect.
    async fn profile(&self, did: &str) -> Option<Profile> {
        let url = format!("{}/xrpc/app.bsky.actor.getProfile", self.appview);
        let res = self.client.get(url).query(&[("actor", did)]).send().await;
        match res {
            Ok(r) if r.status() == StatusCode::OK => r.json().await.ok(),
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
