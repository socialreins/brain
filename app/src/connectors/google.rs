//! Google sign-in (OpenID Connect, authorization code with PKCE). The id_token comes straight
//! from Google's token endpoint over TLS in exchange for our client secret, so its claims are
//! checked (issuer, audience, expiry, nonce, verified email) but not its signature, as OpenID
//! Connect Core 3.1.3.7 allows for a token obtained that way.
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const ISSUERS: [&str; 2] = ["https://accounts.google.com", "accounts.google.com"];

/// `GOOGLE_CLIENT_ID` and `GOOGLE_CLIENT_SECRET`. No `Debug`: it holds the secret.
#[derive(Clone)]
pub struct Google {
    pub client_id: String,
    client_secret: String,
    /// Google's endpoints; tests point them at a mock.
    pub auth_url: String,
    pub token_url: String,
    client: reqwest::Client,
}

/// A Google account with a verified email.
#[derive(Debug, PartialEq, Eq)]
pub struct Identity {
    pub sub: String,
    pub email: String,
    pub name: Option<String>,
}

#[derive(Deserialize)]
struct TokenAnswer {
    id_token: String,
}

impl Google {
    pub fn new(client_id: &str, client_secret: &str) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        Ok(Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            auth_url: AUTH_URL.into(),
            token_url: TOKEN_URL.into(),
            client,
        })
    }

    /// Where `GET /auth/google/start` sends the browser.
    pub fn authorize_url(
        &self,
        redirect_uri: &str,
        state: &str,
        nonce: &str,
        challenge: &str,
    ) -> String {
        let params = [
            ("client_id", self.client_id.as_str()),
            ("redirect_uri", redirect_uri),
            ("response_type", "code"),
            ("scope", "openid email profile"),
            ("state", state),
            ("nonce", nonce),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
            ("prompt", "select_account"),
        ];
        Url::parse_with_params(&self.auth_url, params)
            .map(String::from)
            .unwrap_or_default()
    }

    /// Trades the code for an id_token and checks it. `None` on any failure; the reason is
    /// logged without the code, tokens or claims.
    pub async fn exchange(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        nonce: &str,
    ) -> Option<Identity> {
        let form = [
            ("code", code),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
            ("code_verifier", verifier),
        ];
        let res = match self.client.post(&self.token_url).form(&form).send().await {
            Ok(res) => res,
            Err(e) => {
                tracing::warn!(error = %e.without_url(), "google token call failed");
                return None;
            }
        };
        if !res.status().is_success() {
            tracing::warn!(status = %res.status(), "google refused the code");
            return None;
        }
        let answer: TokenAnswer = match res.json().await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e.without_url(), "google token answer unreadable");
                return None;
            }
        };
        let now = chrono::Utc::now().timestamp();
        match claims(&answer.id_token).map(|c| check(&c, &self.client_id, nonce, now)) {
            Some(Ok(identity)) => Some(identity),
            Some(Err(why)) => {
                tracing::warn!(why, "google id_token refused");
                None
            }
            None => {
                tracing::warn!("google id_token unreadable");
                None
            }
        }
    }
}

/// The payload of a JWT, unverified.
fn claims(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The id_token's claims as OpenID Connect requires, plus a verified email.
fn check(c: &Value, client_id: &str, nonce: &str, now: i64) -> Result<Identity, &'static str> {
    if !c["iss"].as_str().is_some_and(|i| ISSUERS.contains(&i)) {
        return Err("issuer");
    }
    let aud_ok = match &c["aud"] {
        Value::String(a) => a == client_id,
        // Several audiences: ours must be one and the authorized party.
        Value::Array(list) => {
            list.iter().any(|a| a == client_id) && c["azp"].as_str() == Some(client_id)
        }
        _ => false,
    };
    if !aud_ok {
        return Err("audience");
    }
    if !c["exp"].as_i64().is_some_and(|exp| exp > now) {
        return Err("expired");
    }
    if c["nonce"].as_str() != Some(nonce) {
        return Err("nonce");
    }
    // Google sends a boolean; older tokens sent the string "true".
    let verified = c["email_verified"] == true || c["email_verified"] == "true";
    if !verified {
        return Err("email not verified");
    }
    let sub = c["sub"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 255);
    let email = c["email"].as_str().filter(|e| !e.is_empty());
    let (Some(sub), Some(email)) = (sub, email) else {
        return Err("sub or email missing");
    };
    let name = c["name"]
        .as_str()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_owned);
    Ok(Identity {
        sub: sub.into(),
        email: email.into(),
        name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn good() -> Value {
        json!({"iss": "https://accounts.google.com", "aud": "cid", "exp": 2000,
            "nonce": "n1", "email_verified": true, "sub": "123", "email": "a@b.co",
            "name": " Ann "})
    }

    #[test]
    fn good_claims_pass() {
        let id = check(&good(), "cid", "n1", 1000).unwrap();
        assert_eq!(id.sub, "123");
        assert_eq!(id.email, "a@b.co");
        assert_eq!(id.name.as_deref(), Some("Ann"));
        let mut c = good();
        c["iss"] = json!("accounts.google.com");
        c["email_verified"] = json!("true");
        c["aud"] = json!(["other", "cid"]);
        c["azp"] = json!("cid");
        assert!(check(&c, "cid", "n1", 1000).is_ok());
    }

    #[test]
    fn each_bad_claim_is_refused() {
        let cases: [(&str, Value, &str); 9] = [
            ("iss", json!("https://evil.example"), "issuer"),
            ("aud", json!("other"), "audience"),
            ("aud", json!(["other", "cid"]), "audience"),
            ("exp", json!(999), "expired"),
            ("exp", Value::Null, "expired"),
            ("nonce", json!("n2"), "nonce"),
            ("email_verified", json!(false), "email not verified"),
            ("sub", json!(""), "sub or email missing"),
            ("email", Value::Null, "sub or email missing"),
        ];
        for (key, value, why) in cases {
            let mut c = good();
            c[key] = value;
            assert_eq!(check(&c, "cid", "n1", 1000), Err(why), "{key}");
        }
    }

    #[test]
    fn claims_come_from_the_jwt_payload() {
        let payload = URL_SAFE_NO_PAD.encode(good().to_string());
        let c = claims(&format!("e30.{payload}.sig")).unwrap();
        assert_eq!(c["sub"], "123");
        assert!(claims("garbage").is_none());
    }

    #[test]
    fn the_authorize_url_carries_pkce_state_and_nonce() {
        let g = Google::new("cid", "secret").unwrap();
        let url = g.authorize_url("https://api.example/auth/google/callback", "st", "no", "ch");
        let parsed = Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(parsed.host_str(), Some("accounts.google.com"));
        assert_eq!(q["scope"], "openid email profile");
        assert_eq!(q["state"], "st");
        assert_eq!(q["nonce"], "no");
        assert_eq!(q["code_challenge"], "ch");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(!url.contains("secret"));
    }
}
