//! Cloudflare Turnstile: the captcha on the public forms (`POST /auth/link`, `/v1/waitlist`,
//! `/v1/contact`). Off unless `TURNSTILE_SITE_KEY` and `TURNSTILE_SECRET_KEY` are set.
use std::{net::IpAddr, time::Duration};

use axum::http::StatusCode;

use crate::controllers::waitlist::{error, Reply};

const SITEVERIFY: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";

#[derive(Clone)]
pub struct Turnstile {
    client: reqwest::Client,
    secret: String,
    /// Public: the web app renders the widget with it (`GET /auth/providers`).
    pub site_key: String,
    /// Tests only, never read from the environment: where tokens are verified.
    pub verify_url: String,
}

#[derive(serde::Deserialize)]
struct Verdict {
    success: bool,
}

impl Turnstile {
    pub fn new(site_key: &str, secret: &str) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        Ok(Self {
            client,
            secret: secret.into(),
            site_key: site_key.into(),
            verify_url: SITEVERIFY.into(),
        })
    }

    /// `422 captcha_failed` for a missing or rejected token, `503 captcha_unavailable` when
    /// Cloudflare cannot answer (fail closed). Never logs the token or the secret.
    pub async fn verify(&self, token: Option<&str>, ip: Option<IpAddr>) -> Result<(), Reply> {
        let failed = || error(StatusCode::UNPROCESSABLE_ENTITY, "captcha_failed");
        let token = token.map(str::trim).filter(|t| !t.is_empty());
        let Some(token) = token else {
            return Err(failed());
        };
        let mut form = vec![("secret", self.secret.clone()), ("response", token.into())];
        if let Some(ip) = ip {
            form.push(("remoteip", ip.to_string()));
        }
        let unavailable = |why: &str| {
            tracing::warn!(why, "turnstile siteverify unavailable");
            error(StatusCode::SERVICE_UNAVAILABLE, "captcha_unavailable")
        };
        let res = self.client.post(&self.verify_url).form(&form).send().await;
        let res = res.map_err(|e| {
            unavailable(if e.is_timeout() {
                "timeout"
            } else {
                "connection"
            })
        })?;
        let verdict = res
            .json::<Verdict>()
            .await
            .map_err(|_| unavailable("not json"))?;
        if verdict.success {
            Ok(())
        } else {
            Err(failed())
        }
    }
}

/// `verify` when Turnstile is on; with it off any token is ignored.
pub async fn check(
    turnstile: Option<&Turnstile>,
    token: Option<&str>,
    ip: Option<IpAddr>,
) -> Result<(), Reply> {
    match turnstile {
        Some(t) => t.verify(token, ip).await,
        None => Ok(()),
    }
}
