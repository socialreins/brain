//! `Config`: every setting is an environment variable, and its default lives here.
use axum::http::HeaderValue;

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
        Ok(Self {
            database_url,
            port,
            cors_origins: parse_origins(&origins)?,
            git_commit: var("RAILWAY_GIT_COMMIT_SHA").unwrap_or_else(|| "dev".into()),
            trust_proxy: match var("TRUST_PROXY").as_deref() {
                None | Some("false") => false,
                Some("true") => true,
                Some(_) => return Err("TRUST_PROXY: expected true or false".into()),
            },
        })
    }
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
}
