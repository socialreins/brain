//! `Config`: every setting is an environment variable, and its default lives here.
use std::sync::Arc;

use axum::http::HeaderValue;
use webauthn_rs::{prelude::Url, Webauthn, WebauthnBuilder};

use crate::mail::Mailer;

/// No `Debug`: `database_url` carries a password.
#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub port: u16,
    pub cors_origins: Vec<HeaderValue>,
    /// `RAILWAY_GIT_COMMIT_SHA`, or `dev` outside Railway.
    pub git_commit: String,
    /// `TRUST_PROXY=true` only behind a proxy that sets `X-Forwarded-For` (Railway). Off by
    /// default: a client could otherwise spoof the header to dodge the rate limit.
    pub trust_proxy: bool,
    /// `BRAIN_URL`: this API's public base URL, used in emailed links. No trailing slash.
    pub brain_url: String,
    /// `WEB_URL`: the web app's public base URL, where sign-in lands. An `http://` URL turns
    /// off the cookie's `Secure` flag for local dev. No trailing slash.
    pub web_url: String,
    /// `REGISTRATION_DISABLED=true`: magic links stop creating accounts; existing users still sign in.
    pub registration_disabled: bool,
    /// `RESEND_API_KEY`, else `SMTP_URL` (either needs `MAIL_FROM`), else the log, which needs
    /// `MAIL_LOG=true` so sign-in links never reach the log by accident.
    pub mailer: Mailer,
    /// WebAuthn relying party: `PASSKEY_RP_ID` (default: the host of `WEB_URL`) is the domain
    /// web and brain share, `PASSKEY_RP_ORIGIN` (default: the origin of `WEB_URL`) is where the
    /// browser runs the ceremony.
    pub webauthn: Arc<Webauthn>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let database_url = var("DATABASE_URL").ok_or("missing required variable: DATABASE_URL")?;
        let port = match var("PORT") {
            Some(p) => p.parse().map_err(|_| "PORT: not a port number")?,
            None => 8080,
        };
        let flag = |name: &str| match var(name).as_deref() {
            None | Some("false") => Ok(false),
            Some("true") => Ok(true),
            Some(_) => Err(format!("{name}: expected true or false")),
        };
        let from = || var("MAIL_FROM").ok_or("MAIL_FROM: required when mail is configured");
        let mailer = match (var("RESEND_API_KEY"), var("SMTP_URL")) {
            (Some(key), _) => Mailer::resend(&key, &from()?)?,
            (None, Some(url)) => Mailer::smtp(&url, &from()?)?,
            (None, None) if flag("MAIL_LOG")? => Mailer::Log,
            (None, None) => {
                return Err(
                    "no mail configured: set RESEND_API_KEY or SMTP_URL, or MAIL_LOG=true \
                            to write sign-in links to the log (single-user self-host)"
                        .into(),
                )
            }
        };
        let web_url = base_url("WEB_URL", var("WEB_URL"), "http://localhost:3000")?;
        // Default: the web app only.
        let origins = var("CORS_ORIGINS").unwrap_or_else(|| origin(&web_url).into());
        let rp_origin = var("PASSKEY_RP_ORIGIN").unwrap_or_else(|| origin(&web_url).into());
        let webauthn = webauthn(var("PASSKEY_RP_ID"), &rp_origin)?;
        Ok(Self {
            database_url,
            port,
            cors_origins: parse_origins(&origins)?,
            git_commit: var("RAILWAY_GIT_COMMIT_SHA").unwrap_or_else(|| "dev".into()),
            trust_proxy: flag("TRUST_PROXY")?,
            brain_url: base_url("BRAIN_URL", var("BRAIN_URL"), "http://localhost:8080")?,
            web_url,
            registration_disabled: flag("REGISTRATION_DISABLED")?,
            mailer,
            webauthn,
        })
    }
}

fn base_url(name: &str, value: Option<String>, default: &str) -> Result<String, String> {
    let url = value.unwrap_or_else(|| default.into());
    let url = url.trim().trim_end_matches('/');
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("{name}: expected an http:// or https:// URL"));
    }
    Ok(url.into())
}

fn webauthn(rp_id: Option<String>, rp_origin: &str) -> Result<Arc<Webauthn>, String> {
    let url = Url::parse(rp_origin).map_err(|_| "PASSKEY_RP_ORIGIN: not a URL")?;
    let rp_id = rp_id.unwrap_or_else(|| url.host_str().unwrap_or_default().into());
    WebauthnBuilder::new(&rp_id, &url)
        .and_then(|b| b.rp_name("SocialReins").build())
        .map(Arc::new)
        .map_err(|_| {
            "PASSKEY_RP_ID: must be the host of PASSKEY_RP_ORIGIN or a parent domain".into()
        })
}

/// `scheme://host[:port]` of an absolute URL, e.g. the origin a browser sends for it.
pub fn origin(url: &str) -> &str {
    let host_start = url.find("://").map_or(0, |i| i + 3);
    url[host_start..]
        .find(['/', '?', '#'])
        .map_or(url, |end| &url[..host_start + end])
}

/// Comma-separated origins, e.g. `https://a.com, http://localhost:3000`.
fn parse_origins(list: &str) -> Result<Vec<HeaderValue>, String> {
    list.split(',')
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .map(|o| HeaderValue::from_str(o).map_err(|_| format!("CORS_ORIGINS: invalid origin {o}")))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn origins_split_and_trim() {
        let o = super::parse_origins(" https://a.com ,http://localhost:3000,").unwrap();
        assert_eq!(o, ["https://a.com", "http://localhost:3000"]);
    }

    #[test]
    fn origin_drops_the_path() {
        assert_eq!(
            super::origin("https://a.com:8443/x/y?z"),
            "https://a.com:8443"
        );
        assert_eq!(
            super::origin("http://localhost:3000"),
            "http://localhost:3000"
        );
        assert_eq!(
            super::origin("https://a.com.evil.test/"),
            "https://a.com.evil.test"
        );
    }

    #[test]
    fn passkey_rp_id_must_cover_the_origin() {
        assert!(super::webauthn(None, "http://localhost:3000").is_ok());
        let id = |v: &str| super::webauthn(Some(v.into()), "https://app.example.com");
        assert!(id("example.com").is_ok());
        assert!(id("other.com").is_err());
        assert!(id("ample.com").is_err());
    }

    #[test]
    fn base_urls_drop_the_trailing_slash_and_need_a_scheme() {
        let url = |v: &str| super::base_url("X", Some(v.into()), "http://d");
        assert_eq!(url("https://a.com/").unwrap(), "https://a.com");
        assert!(url("a.com").is_err());
        assert_eq!(super::base_url("X", None, "http://d").unwrap(), "http://d");
    }
}
