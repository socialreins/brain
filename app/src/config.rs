//! `Config`: every setting is an environment variable, and its default lives here.
use axum::http::HeaderValue;

use crate::mail::Mailer;

const DEFAULT_CORS_ORIGINS: &str =
    "https://socialreins.com,https://www.socialreins.com,http://localhost:3000";

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
    /// `RESEND_API_KEY`, else `SMTP_URL`, else the log. Either needs `MAIL_FROM`.
    pub mailer: Mailer,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let database_url = var("DATABASE_URL").ok_or("missing required variable: DATABASE_URL")?;
        let port = match var("PORT") {
            Some(p) => p.parse().map_err(|_| "PORT: not a port number")?,
            None => 8080,
        };
        let origins = var("CORS_ORIGINS").unwrap_or_else(|| DEFAULT_CORS_ORIGINS.into());
        let flag = |name: &str| match var(name).as_deref() {
            None | Some("false") => Ok(false),
            Some("true") => Ok(true),
            Some(_) => Err(format!("{name}: expected true or false")),
        };
        let from = || var("MAIL_FROM").ok_or("MAIL_FROM: required when mail is configured");
        let mailer = match (var("RESEND_API_KEY"), var("SMTP_URL")) {
            (Some(key), _) => Mailer::resend(&key, &from()?)?,
            (None, Some(url)) => Mailer::smtp(&url, &from()?)?,
            (None, None) => Mailer::Log,
        };
        Ok(Self {
            database_url,
            port,
            cors_origins: parse_origins(&origins)?,
            git_commit: var("RAILWAY_GIT_COMMIT_SHA").unwrap_or_else(|| "dev".into()),
            trust_proxy: flag("TRUST_PROXY")?,
            brain_url: base_url("BRAIN_URL", var("BRAIN_URL"), "http://localhost:8080")?,
            web_url: base_url("WEB_URL", var("WEB_URL"), "http://localhost:3000")?,
            registration_disabled: flag("REGISTRATION_DISABLED")?,
            mailer,
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
    fn base_urls_drop_the_trailing_slash_and_need_a_scheme() {
        let url = |v: &str| super::base_url("X", Some(v.into()), "http://d");
        assert_eq!(url("https://a.com/").unwrap(), "https://a.com");
        assert!(url("a.com").is_err());
        assert_eq!(super::base_url("X", None, "http://d").unwrap(), "http://d");
    }
}
