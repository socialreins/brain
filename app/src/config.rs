//! `Config`: every setting is an environment variable, and its default lives here.
use std::sync::Arc;

use axum::http::HeaderValue;
use object_store::{aws::AmazonS3Builder, local::LocalFileSystem, ObjectStore};
use webauthn_rs::{prelude::Url, Webauthn, WebauthnBuilder};

use socialreins_core::crypto::Cipher;

use crate::{
    connectors::{bluesky::Bluesky, google::Google, meta, threads::Threads},
    mail::Mailer,
    media::Media,
    turnstile::Turnstile,
};

/// Public Bluesky AppView (handle resolution, profiles) and the `did:plc` directory.
const BLUESKY_APPVIEW: &str = "https://public.api.bsky.app";
const PLC_DIRECTORY: &str = "https://plc.directory";

/// Each OAuth provider and the env vars of its app; it is offered once both are set.
const PROVIDER_APPS: [(&str, [&str; 2]); 6] = [
    ("meta", ["META_APP_ID", "META_APP_SECRET"]),
    ("threads", ["THREADS_APP_ID", "THREADS_APP_SECRET"]),
    ("linkedin", ["LINKEDIN_CLIENT_ID", "LINKEDIN_CLIENT_SECRET"]),
    ("x", ["X_CLIENT_ID", "X_CLIENT_SECRET"]),
    ("tiktok", ["TIKTOK_CLIENT_KEY", "TIKTOK_CLIENT_SECRET"]),
    ("youtube", ["GOOGLE_CLIENT_ID", "GOOGLE_CLIENT_SECRET"]),
];

/// No `Debug`: `database_url` carries a password.
#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub port: u16,
    pub cors_origins: Vec<HeaderValue>,
    /// `RAILWAY_GIT_COMMIT_SHA`, else `GIT_COMMIT` (baked into published images), else `dev`.
    pub git_commit: String,
    /// `TRUST_PROXY=true` only behind a proxy that sets `X-Forwarded-For` (Railway). Off by
    /// default: a client could otherwise spoof the header to dodge the rate limit.
    pub trust_proxy: bool,
    /// `BRAIN_URL`: this API's public base URL, used in emailed links. No trailing slash.
    pub brain_url: String,
    /// `WEB_URL`: the web app's public base URL, where sign-in lands. An `http://` URL turns
    /// off the cookie's `Secure` flag for local dev. No trailing slash.
    pub web_url: String,
    /// `REGISTRATION_DISABLED=true`: magic links create only `OWNER_EMAIL`'s account (or, with
    /// no `OWNER_EMAIL`, the first account) and invited ones; existing users still sign in.
    pub registration_disabled: bool,
    /// `OWNER_EMAIL`, normalized: the one stranger a closed install still lets sign up.
    pub owner_email: Option<String>,
    /// `RESEND_API_KEY`, else `SMTP_URL` (either needs `MAIL_FROM`), else the log, which needs
    /// `MAIL_LOG=true` so sign-in links never reach the log by accident.
    pub mailer: Mailer,
    /// WebAuthn relying party: `PASSKEY_RP_ID` (default: the host of `WEB_URL`) is the domain
    /// web and brain share, `PASSKEY_RP_ORIGIN` (default: the origin of `WEB_URL`) is where the
    /// browser runs the ceremony.
    pub webauthn: Arc<Webauthn>,
    /// `ENCRYPTION_KEY` (required): 32 bytes, base64. Seals platform tokens at rest.
    pub cipher: Arc<Cipher>,
    pub bluesky: Bluesky,
    /// Every provider and whether it can be connected: Bluesky always, the others once their
    /// app id and secret are set (`PROVIDER_APPS`).
    pub providers: Vec<(&'static str, bool)>,
    /// `GOOGLE_CLIENT_ID` and `GOOGLE_CLIENT_SECRET` (both or neither): Google sign-in, with
    /// the redirect URI `{BRAIN_URL}/auth/google/callback`.
    pub google: Option<Google>,
    /// `TURNSTILE_SITE_KEY` and `TURNSTILE_SECRET_KEY` (both or neither): a Cloudflare
    /// Turnstile captcha on sign-in links and the contact form. Off when unset.
    pub turnstile: Option<Turnstile>,
    /// `CONTACT_TO`, normalized: where `POST /v1/contact` mails messages. Off when unset.
    pub contact_to: Option<String>,
    /// `ADMIN_EMAILS`, comma-separated and normalized: users who may read `/v1/admin/*`.
    /// Empty when unset.
    pub admin_emails: Vec<String>,
    /// Where post media is stored while a post publishes. `S3_BUCKET`, `S3_ENDPOINT`,
    /// `S3_ACCESS_KEY_ID` and `S3_SECRET_ACCESS_KEY` (all or none; `S3_REGION`, default `auto`):
    /// an S3-compatible bucket, path-style (Cloudflare R2, AWS S3, MinIO). Else local disk at
    /// `MEDIA_DIR` (default `./media`, created when missing).
    pub media: Media,
    /// `THREADS_APP_ID` and `THREADS_APP_SECRET`: connecting Threads accounts, with the
    /// redirect URI `{BRAIN_URL}/connect/threads/callback`. Publishing with a connected
    /// account's token needs neither.
    pub threads: Threads,
    /// `META_APP_ID` and `META_APP_SECRET`: connecting Facebook Pages and Instagram accounts,
    /// with the redirect URI `{BRAIN_URL}/connect/meta/callback`, on Graph API
    /// `META_GRAPH_VERSION` (default `v26.0`). Publishing with a stored Page token needs neither.
    /// `META_WEBHOOK_VERIFY_TOKEN` turns on `GET /webhooks/meta` (the handshake when the webhook
    /// is registered in the Meta app); `POST /webhooks/meta` is signed with `META_APP_SECRET`.
    pub meta: meta::Meta,
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
            // Fail closed: a lost RESEND_API_KEY must not quietly put sign-in links in the log.
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
        let cipher = cipher(var("ENCRYPTION_KEY"))?;
        let google = match (var("GOOGLE_CLIENT_ID"), var("GOOGLE_CLIENT_SECRET")) {
            (Some(id), Some(secret)) => Some(Google::new(&id, &secret)?),
            (None, None) => None,
            _ => {
                return Err("GOOGLE_CLIENT_ID and GOOGLE_CLIENT_SECRET: set both or neither".into())
            }
        };
        let turnstile = match (var("TURNSTILE_SITE_KEY"), var("TURNSTILE_SECRET_KEY")) {
            (Some(site), Some(secret)) => Some(Turnstile::new(&site, &secret)?),
            (None, None) => None,
            _ => {
                return Err(
                    "TURNSTILE_SITE_KEY and TURNSTILE_SECRET_KEY: set both or neither".into(),
                )
            }
        };
        let contact_to = var("CONTACT_TO")
            .map(|e| {
                socialreins_core::email::normalize(&e).ok_or("CONTACT_TO: not an email address")
            })
            .transpose()?;
        let mut providers = vec![("bluesky", true)];
        providers.extend(
            PROVIDER_APPS.map(|(name, vars)| (name, vars.iter().all(|v| var(v).is_some()))),
        );
        Ok(Self {
            database_url,
            port,
            cors_origins: parse_origins(&origins)?,
            git_commit: var("RAILWAY_GIT_COMMIT_SHA")
                .or_else(|| var("GIT_COMMIT"))
                .unwrap_or_else(|| "dev".into()),
            trust_proxy: flag("TRUST_PROXY")?,
            brain_url: base_url("BRAIN_URL", var("BRAIN_URL"), "http://localhost:8080")?,
            web_url,
            registration_disabled: flag("REGISTRATION_DISABLED")?,
            owner_email: owner_email(var("OWNER_EMAIL"))?,
            mailer,
            webauthn,
            cipher: Arc::new(cipher),
            bluesky: Bluesky::new(BLUESKY_APPVIEW, PLC_DIRECTORY)?,
            providers,
            google,
            turnstile,
            contact_to,
            admin_emails: admin_emails(var("ADMIN_EMAILS"))?,
            media: Media::new(media_store(var)?, false),
            threads: Threads::new(var("THREADS_APP_ID").zip(var("THREADS_APP_SECRET")))?,
            meta: {
                let mut meta = meta::Meta::new(
                    var("META_APP_ID").zip(var("META_APP_SECRET")),
                    &var("META_GRAPH_VERSION").unwrap_or_else(|| meta::DEFAULT_VERSION.into()),
                )?;
                meta.webhook_token = var("META_WEBHOOK_VERIFY_TOKEN");
                meta
            },
        })
    }
}

/// `OWNER_EMAIL`, trimmed and lowercased like every sign-in email, so it matches in any case.
fn owner_email(raw: Option<String>) -> Result<Option<String>, &'static str> {
    raw.map(|e| socialreins_core::email::normalize(&e).ok_or("OWNER_EMAIL: not an email address"))
        .transpose()
}

/// `ADMIN_EMAILS`: each address normalized like `OWNER_EMAIL`; blank entries are skipped.
fn admin_emails(raw: Option<String>) -> Result<Vec<String>, String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|e| {
            socialreins_core::email::normalize(e)
                .ok_or_else(|| format!("ADMIN_EMAILS: not an email address: {e}"))
        })
        .collect()
}

/// The media store `Config::media` describes.
fn media_store(var: impl Fn(&str) -> Option<String>) -> Result<Arc<dyn ObjectStore>, String> {
    let s3 = [
        "S3_BUCKET",
        "S3_ENDPOINT",
        "S3_ACCESS_KEY_ID",
        "S3_SECRET_ACCESS_KEY",
    ]
    .map(&var);
    match s3 {
        [Some(bucket), Some(endpoint), Some(key_id), Some(secret)] => {
            let s3 = AmazonS3Builder::new()
                .with_bucket_name(bucket)
                .with_endpoint(endpoint)
                .with_region(var("S3_REGION").unwrap_or_else(|| "auto".into()))
                .with_access_key_id(key_id)
                .with_secret_access_key(secret)
                .build()
                .map_err(|e| format!("S3_BUCKET: {e}"))?;
            Ok(Arc::new(s3))
        }
        [None, None, None, None] => {
            let dir = var("MEDIA_DIR").unwrap_or_else(|| "./media".into());
            std::fs::create_dir_all(&dir).map_err(|e| format!("MEDIA_DIR: {e}"))?;
            let local =
                LocalFileSystem::new_with_prefix(&dir).map_err(|e| format!("MEDIA_DIR: {e}"))?;
            Ok(Arc::new(local.with_automatic_cleanup(true)))
        }
        _ => Err(
            "S3_BUCKET, S3_ENDPOINT, S3_ACCESS_KEY_ID and S3_SECRET_ACCESS_KEY: set all \
                  or none"
                .into(),
        ),
    }
}

/// `ENCRYPTION_KEY`: brain does not start without a valid one.
fn cipher(key: Option<String>) -> Result<Cipher, &'static str> {
    let key = key.ok_or(
        "missing required variable: ENCRYPTION_KEY (generate one: openssl rand -base64 32)",
    )?;
    Cipher::from_base64(&key).ok_or("ENCRYPTION_KEY: expected 32 bytes, base64")
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
    fn owner_email_is_normalized_and_checked() {
        let owner = |v: &str| super::owner_email(Some(v.into()));
        assert_eq!(
            owner(" Me@Example.COM ").unwrap().as_deref(),
            Some("me@example.com")
        );
        assert!(owner("not-an-email").is_err());
        assert_eq!(super::owner_email(None).unwrap(), None);
    }

    #[test]
    fn admin_emails_split_and_normalize() {
        let admins = |v: &str| super::admin_emails(Some(v.into()));
        assert_eq!(
            admins(" A@Example.com, ,b@example.com,").unwrap(),
            ["a@example.com", "b@example.com"]
        );
        assert!(admins("a@example.com,nope").is_err());
        assert!(super::admin_emails(None).unwrap().is_empty());
    }

    #[test]
    fn encryption_key_is_required_and_checked() {
        assert!(super::cipher(None).is_err());
        assert!(super::cipher(Some("c2hvcnQ=".into())).is_err());
        let key = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
        assert!(super::cipher(Some(key.into())).is_ok());
    }

    #[test]
    fn media_is_on_disk_unless_the_whole_bucket_is_set() {
        let dir = std::env::temp_dir().join(format!("media-{}", uuid::Uuid::now_v7()));
        let dir = dir.to_str().unwrap().to_owned();
        let with = |pairs: &[(&str, &str)], dir: &str| {
            let mut vars: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            vars.push(("MEDIA_DIR".into(), dir.to_owned()));
            super::media_store(move |n: &str| {
                vars.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone())
            })
        };
        let local = with(&[], &dir).ok().unwrap();
        assert!(local.to_string().contains("LocalFileSystem"), "{local}");
        assert!(std::path::Path::new(&dir).is_dir());
        let full = [
            ("S3_BUCKET", "media"),
            ("S3_ENDPOINT", "https://acct.r2.cloudflarestorage.com"),
            ("S3_ACCESS_KEY_ID", "id"),
            ("S3_SECRET_ACCESS_KEY", "secret"),
        ];
        let s3 = with(&full, &dir).ok().unwrap();
        assert!(s3.to_string().contains("media"), "{s3}");
        for n in 0..full.len() {
            let mut partial = full.to_vec();
            partial.remove(n);
            assert!(with(&partial, &dir).is_err(), "without {}", full[n].0);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn base_urls_drop_the_trailing_slash_and_need_a_scheme() {
        let url = |v: &str| super::base_url("X", Some(v.into()), "http://d");
        assert_eq!(url("https://a.com/").unwrap(), "https://a.com");
        assert!(url("a.com").is_err());
        assert_eq!(super::base_url("X", None, "http://d").unwrap(), "http://d");
    }
}
