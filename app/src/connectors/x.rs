//! X (an account): OAuth 2.0 with PKCE as a confidential client, token refresh, the profile,
//! posting with API v2 (`POST /2/tweets`, replies for thread parts), media through the v2 chunked
//! upload (initialize, append, finalize, status) and take down. A post is text with up to four
//! images, or one GIF, or one video. Access tokens last two hours and refresh tokens are single
//! use, so renewing is done under a row lock (`controllers::x`). Errors carry no URL, token or
//! secret.
use std::time::Duration;

use chrono::DateTime;
use reqwest::{header::HeaderMap, Method, RequestBuilder, StatusCode, Url};
use serde::Deserialize;
use serde_json::{json, Value};

use super::bluesky::PostError;

const AUTH_URL: &str = "https://x.com/i/oauth2/authorize";
const API_URL: &str = "https://api.x.com";
/// `media.write` is what the v2 media upload endpoints need.
const SCOPES: &str = "tweet.read tweet.write users.read offline.access media.write";
/// Used when X leaves `expires_in` out.
const DEFAULT_EXPIRES_IN: i64 = 7200;
/// Media status checks, each after the wait X asks for (at most `MAX_WAIT_SECS`).
const POLLS: u32 = 20;
const MAX_WAIT_SECS: u64 = 15;
/// Longest one post (uploads, video processing and the tweet) may take.
pub const PUBLISH_TIMEOUT: Duration = Duration::from_secs(420);
/// Bytes per append call; X allows 8 MB and advises 5.
const CHUNK: usize = 4_000_000;
pub const IMAGE_TYPES: [&str; 3] = ["image/jpeg", "image/png", "image/webp"];
pub const GIF_TYPE: &str = "image/gif";
pub const VIDEO_TYPES: [&str; 2] = ["video/mp4", "video/quicktime"];
const MAX_IMAGE_BYTES: usize = socialreins_core::post::X_MAX_IMAGE_BYTES as usize;
const MAX_GIF_BYTES: usize = 15_000_000;
const MAX_VIDEO_BYTES: usize = 512_000_000;
/// Images a tweet may hold.
pub const MAX_IMAGES: usize = 4;

const CREDITS: &str = "X says this app is out of API credits (X bills API use by credits). Add \
                       credits in the X developer console, then publish again.";
const USAGE_CAPPED: &str = "X's usage cap for this app is reached. Raise the cap in the X \
                            developer console, or wait for it to reset.";

/// The X app (`X_CLIENT_ID`, `X_CLIENT_SECRET`). No `Debug`: it holds the secret.
#[derive(Clone)]
pub struct X {
    /// `(client id, client secret)`; connecting and renewing need them, calls with a live access
    /// token do not.
    app: Option<(String, String)>,
    /// Tests point these at a mock, and shorten `poll_every`.
    pub auth_url: String,
    pub api_url: String,
    pub poll_every: Duration,
    client: reqwest::Client,
}

pub struct Token {
    pub access_token: String,
    pub expires_in: i64,
    /// Sent by the code swap, and by a refresh unless X keeps the current one.
    pub refresh_token: Option<String>,
    /// The scopes X granted, space-separated. The user can untick some.
    pub scope: Option<String>,
}

impl Token {
    /// Whether every scope this connector asks for was granted. A token that names none counts
    /// as missing them, so the connect fails rather than breaking at the first publish.
    pub fn has_all_scopes(&self) -> bool {
        let granted: Vec<&str> = self
            .scope
            .as_deref()
            .unwrap_or_default()
            .split([' ', ','])
            .collect();
        SCOPES.split(' ').all(|s| granted.contains(&s))
    }
}

#[derive(Debug, Deserialize)]
pub struct Profile {
    pub id: String,
    pub username: String,
    pub name: Option<String>,
    pub profile_image_url: Option<String>,
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: String,
    expires_in: Option<i64>,
    refresh_token: Option<String>,
    scope: Option<String>,
}

/// One file to post: its bytes (a video's are read from the store a chunk at a time) and type.
pub struct File<'a> {
    pub bytes: &'a crate::media::Blob,
    pub content_type: &'a str,
}

fn failed(msg: impl Into<String>) -> PostError {
    PostError::Failed(msg.into())
}

/// X's own words for a refused call, without links, cut short.
fn reason(body: &Value, status: StatusCode) -> String {
    let msg = body["detail"]
        .as_str()
        .or(body["errors"][0]["message"].as_str())
        .or(body["error_description"].as_str())
        .or(body["title"].as_str())
        .unwrap_or_default();
    let words: Vec<&str> = msg
        .split_whitespace()
        .filter(|w| !w.contains("://") && !w.contains("access_token"))
        .collect();
    let msg: String = words.join(" ").chars().take(200).collect();
    if msg.is_empty() {
        format!("HTTP {}", status.as_u16())
    } else {
        msg
    }
}

/// Whether X says the app has no credits left: 402, or an error whose type, title or detail
/// speaks of credits (e.g. `CreditsDepleted`).
fn out_of_credits(status: StatusCode, body: &Value) -> bool {
    let said = ["type", "title", "detail"]
        .iter()
        .filter_map(|k| body[*k].as_str())
        .any(|s| s.to_ascii_lowercase().contains("credit"));
    status == StatusCode::PAYMENT_REQUIRED || said
}

/// What a refused API call says to the user: out of credits, the usage cap, the rate limit with
/// when it resets (X sends `x-rate-limit-reset` as Unix seconds), or X's reason.
fn describe(status: StatusCode, headers: &HeaderMap, body: &Value) -> String {
    if out_of_credits(status, body) {
        return CREDITS.into();
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        if body["type"]
            .as_str()
            .is_some_and(|t| t.ends_with("usage-capped"))
        {
            return USAGE_CAPPED.into();
        }
        let reset = headers
            .get("x-rate-limit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok())
            .and_then(|s| DateTime::from_timestamp(s, 0));
        return match reset {
            Some(at) => format!(
                "X rate limit reached; it resets at {} UTC. It was not retried.",
                at.format("%Y-%m-%d %H:%M")
            ),
            None => "X rate limit reached. It was not retried.".into(),
        };
    }
    reason(body, status)
}

/// What a refused call means. On the token endpoint a 400 is a refused code or refresh token
/// (`Reconnect`) and a 401 is the app's own client secret (a plain failure); on the API a 401 is
/// a dead or revoked token (`Reconnect`). Anything else is `Failed` with `step` and X's reason.
fn refusal(
    status: StatusCode,
    headers: &HeaderMap,
    body: &Value,
    step: &str,
    token_endpoint: bool,
) -> PostError {
    let dead = if token_endpoint {
        status == StatusCode::BAD_REQUEST
    } else {
        status == StatusCode::UNAUTHORIZED
    };
    if dead {
        return PostError::Reconnect;
    }
    failed(format!("{step}: {}", describe(status, headers, body)))
}

/// A numeric id (tweet or media) as one path segment. Only plain ids pass.
fn plain_id(id: &str) -> Option<&str> {
    (!id.is_empty() && id.len() <= 40 && id.bytes().all(|b| b.is_ascii_digit())).then_some(id)
}

/// The public link to a post. A handle that is not a plain X username is left out: `/i/status`
/// works without one.
pub fn status_url(handle: Option<&str>, id: &str) -> String {
    let user = handle
        .filter(|h| !h.is_empty() && h.len() <= 15)
        .filter(|h| h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        .unwrap_or("i");
    format!("https://x.com/{user}/status/{id}")
}

/// A `multipart/form-data` body of one text field and one file field, and its boundary.
fn multipart(field: (&str, &str), file: (&str, &[u8])) -> (String, Vec<u8>) {
    let boundary = format!("srx{}", uuid::Uuid::now_v7().simple());
    let mut body = Vec::with_capacity(file.1.len() + 512);
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n{}\r\n\
             --{boundary}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"chunk\"\r\n\
             Content-Type: application/octet-stream\r\n\r\n",
            field.0, field.1, file.0
        )
        .as_bytes(),
    );
    body.extend_from_slice(file.1);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

impl X {
    pub fn new(app: Option<(String, String)>) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("SocialReins")
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        Ok(Self {
            app,
            auth_url: AUTH_URL.into(),
            api_url: API_URL.into(),
            poll_every: Duration::from_secs(1),
            client,
        })
    }

    /// Whether the client id and secret are set, so accounts can be connected.
    pub fn configured(&self) -> bool {
        self.app.is_some()
    }

    /// Where `GET /connections/x/redirect` sends the browser; `None` when not configured.
    /// `challenge` is the S256 challenge of the verifier the caller keeps.
    pub fn authorize_url(
        &self,
        redirect_uri: &str,
        state: &str,
        challenge: &str,
    ) -> Option<String> {
        let (id, _) = self.app.as_ref()?;
        let params = [
            ("response_type", "code"),
            ("client_id", id.as_str()),
            ("redirect_uri", redirect_uri),
            ("scope", SCOPES),
            ("state", state),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ];
        Url::parse_with_params(&self.auth_url, params)
            .ok()
            .map(String::from)
    }

    /// Sends a call and returns X's answer whatever its status; only a failure to reach X is an
    /// error.
    async fn call(
        &self,
        req: RequestBuilder,
        step: &str,
    ) -> Result<(StatusCode, HeaderMap, Value), PostError> {
        let res = req.send().await.map_err(|e| {
            tracing::warn!(error = %e.without_url(), step, "x call failed");
            failed(format!("{step}: X could not be reached."))
        })?;
        let status = res.status();
        let headers = res.headers().clone();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        Ok((status, headers, body))
    }

    /// `call`, with a refusal turned into its error (see `refusal`).
    async fn send(
        &self,
        req: RequestBuilder,
        step: &str,
        token_endpoint: bool,
    ) -> Result<Value, PostError> {
        let (status, headers, body) = self.call(req, step).await?;
        if status.is_success() {
            return Ok(body);
        }
        Err(refusal(status, &headers, &body, step, token_endpoint))
    }

    fn api(&self, method: Method, path: &str, token: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.api_url))
            .bearer_auth(token)
    }

    async fn token(&self, form: &[(&str, &str)], step: &str) -> Result<Token, PostError> {
        let Some((id, secret)) = &self.app else {
            return Err(failed("X is not set up."));
        };
        let req = self
            .client
            .post(format!("{}/2/oauth2/token", self.api_url))
            .basic_auth(id, Some(secret))
            .form(form);
        let body = self.send(req, step, true).await?;
        let t: TokenAnswer = serde_json::from_value(body)
            .map_err(|_| failed(format!("{step}: X sent no token.")))?;
        Ok(Token {
            access_token: t.access_token,
            expires_in: t
                .expires_in
                .filter(|s| *s > 0)
                .unwrap_or(DEFAULT_EXPIRES_IN),
            refresh_token: t.refresh_token.filter(|r| !r.is_empty()),
            scope: t.scope,
        })
    }

    /// Swaps an authorization code (and the PKCE `verifier` of its attempt) for a token.
    pub async fn exchange(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<Token, PostError> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ];
        self.token(&form, "Code").await
    }

    /// Spends a refresh token. It is single use: the caller must save the new one it gets (and
    /// keep the old when none comes) before anything else can spend the old.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Token, PostError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        self.token(&form, "Refresh").await
    }

    /// The signed-in account.
    pub async fn me(&self, access_token: &str) -> Result<Profile, PostError> {
        let req = self
            .api(Method::GET, "/2/users/me", access_token)
            .query(&[("user.fields", "profile_image_url,username,name")]);
        let body = self.send(req, "Profile", false).await?;
        let me: Profile = serde_json::from_value(body["data"].clone())
            .map_err(|_| failed("Profile: X did not say who signed in."))?;
        if plain_id(&me.id).is_none() || me.username.is_empty() {
            return Err(failed("Profile: X did not say who signed in."));
        }
        Ok(me)
    }

    /// Posts a tweet: text and up to four images, or one GIF, or one video, as a reply to
    /// `reply_to` when given. Returns the tweet's id. Nothing is retried.
    pub async fn publish_post(
        &self,
        token: &str,
        text: &str,
        media: &[File<'_>],
        reply_to: Option<&str>,
    ) -> Result<String, PostError> {
        let gifs = media.iter().filter(|m| m.content_type == GIF_TYPE).count();
        let videos = media
            .iter()
            .filter(|m| VIDEO_TYPES.contains(&m.content_type))
            .count();
        if media.len() > MAX_IMAGES || ((gifs > 0 || videos > 0) && media.len() > 1) {
            return Err(failed(
                "X takes up to 4 images, or one GIF, or one video, but not a mix.",
            ));
        }
        let mut ids = Vec::new();
        for m in media {
            ids.push(self.upload(token, m).await?);
        }
        let mut body = json!({});
        if !text.is_empty() {
            body["text"] = json!(text);
        }
        if !ids.is_empty() {
            body["media"] = json!({"media_ids": ids});
        }
        if let Some(parent) = reply_to {
            body["reply"] = json!({"in_reply_to_tweet_id": parent});
        }
        let req = self.api(Method::POST, "/2/tweets", token).json(&body);
        let answer = self.send(req, "Publish", false).await?;
        answer["data"]["id"]
            .as_str()
            .and_then(plain_id)
            .map(str::to_owned)
            .ok_or_else(|| failed("Publish: X sent no post id."))
    }

    /// Uploads one file in chunks and waits until X has processed it. Returns the media id.
    async fn upload(&self, token: &str, file: &File<'_>) -> Result<String, PostError> {
        let (category, max) = match file.content_type {
            t if IMAGE_TYPES.contains(&t) => ("tweet_image", MAX_IMAGE_BYTES),
            GIF_TYPE => ("tweet_gif", MAX_GIF_BYTES),
            t if VIDEO_TYPES.contains(&t) => ("tweet_video", MAX_VIDEO_BYTES),
            _ => {
                return Err(failed(
                    "Upload: X takes JPEG, PNG, WebP and GIF images and MP4 or MOV videos.",
                ))
            }
        };
        let len = file.bytes.len();
        if len > max as u64 {
            let mb = max / 1_000_000;
            return Err(failed(format!(
                "Upload: the file is over {mb} MB, X's limit."
            )));
        }
        let init = json!({
            "media_type": file.content_type,
            "total_bytes": len,
            "media_category": category,
        });
        let req = self
            .api(Method::POST, "/2/media/upload/initialize", token)
            .json(&init);
        let answer = self.send(req, "Upload", false).await?;
        let Some(id) = answer["data"]["id"].as_str().and_then(plain_id) else {
            return Err(failed("Upload: X sent no media id."));
        };
        // One chunk is held at a time, read from the store by its range.
        for (i, at) in (0..len).step_by(CHUNK).enumerate() {
            let chunk = file.bytes.range(at..at + CHUNK as u64).await?;
            let (content_type, body) =
                multipart(("segment_index", &i.to_string()), ("media", &chunk));
            let req = self
                .api(Method::POST, &format!("/2/media/upload/{id}/append"), token)
                .header("Content-Type", content_type)
                .body(body);
            self.send(req, "Upload", false).await?;
        }
        let req = self.api(
            Method::POST,
            &format!("/2/media/upload/{id}/finalize"),
            token,
        );
        let answer = self.send(req, "Upload", false).await?;
        self.processed(id, token, &answer["data"]["processing_info"])
            .await?;
        Ok(id.to_owned())
    }

    /// Waits until X has processed the media; no `processing_info` means it is ready.
    async fn processed(&self, id: &str, token: &str, first: &Value) -> Result<(), PostError> {
        let mut info = first.clone();
        for _ in 0..POLLS {
            match info["state"].as_str() {
                None | Some("succeeded") => return Ok(()),
                Some("failed") => {
                    let why = info["error"]["message"].as_str().unwrap_or("failed");
                    let why: String = why.chars().take(200).collect();
                    return Err(failed(format!("X could not process the media: {why}")));
                }
                _ => {}
            }
            let asked = info["check_after_secs"].as_u64().unwrap_or(1);
            let wait = Duration::from_secs(asked.min(MAX_WAIT_SECS)).max(self.poll_every);
            tokio::time::sleep(wait.min(Duration::from_secs(MAX_WAIT_SECS))).await;
            let req = self
                .api(Method::GET, "/2/media/upload", token)
                .query(&[("command", "STATUS"), ("media_id", id)]);
            let answer = self.send(req, "Status", false).await?;
            info = answer["data"]["processing_info"].clone();
        }
        Err(failed("X did not finish processing the media in time."))
    }

    /// Takes a tweet down (`tweet.write`). A tweet already gone counts as down.
    pub async fn delete_post(&self, token: &str, id: &str) -> Result<(), PostError> {
        let Some(id) = plain_id(id) else {
            return Err(failed("This is not an X post id."));
        };
        let req = self.api(Method::DELETE, &format!("/2/tweets/{id}"), token);
        let (status, headers, body) = self.call(req, "Delete").await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        if !status.is_success() {
            return Err(refusal(status, &headers, &body, "Delete", false));
        }
        if body["data"]["deleted"].as_bool() == Some(true) {
            Ok(())
        } else {
            Err(failed("Delete: X did not delete the post."))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(reset: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(r) = reset {
            h.insert("x-rate-limit-reset", r.parse().unwrap());
        }
        h
    }

    #[test]
    fn the_authorize_url_carries_pkce_the_scopes_and_state() {
        let x = X::new(Some(("app".into(), "secret".into()))).unwrap();
        let url = x
            .authorize_url("https://api.example/connect/x/callback", "st", "chal")
            .unwrap();
        let parsed = Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(parsed.host_str(), Some("x.com"));
        assert_eq!(parsed.path(), "/i/oauth2/authorize");
        assert_eq!(q["client_id"], "app");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["state"], "st");
        assert_eq!(q["code_challenge"], "chal");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(
            q["scope"],
            "tweet.read tweet.write users.read offline.access media.write"
        );
        assert!(!url.contains("secret"));
        assert!(X::new(None)
            .unwrap()
            .authorize_url("https://x", "st", "c")
            .is_none());
    }

    #[test]
    fn every_scope_must_be_granted() {
        let t = |scope: Option<&str>| Token {
            access_token: String::new(),
            expires_in: 1,
            refresh_token: None,
            scope: scope.map(str::to_owned),
        };
        assert!(t(Some(SCOPES)).has_all_scopes());
        assert!(t(Some(
            "media.write offline.access users.read tweet.write tweet.read"
        ))
        .has_all_scopes());
        assert!(!t(Some("tweet.read tweet.write users.read offline.access")).has_all_scopes());
        assert!(!t(Some("tweet.read tweet.write users.read media.write")).has_all_scopes());
        assert!(!t(None).has_all_scopes());
    }

    #[test]
    fn ids_and_links_are_plain() {
        assert_eq!(plain_id("1846987139428636"), Some("1846987139428636"));
        assert_eq!(plain_id(""), None);
        assert_eq!(plain_id("12/../x"), None);
        assert_eq!(plain_id("12a"), None);
        assert_eq!(
            status_url(Some("ann_lee"), "123"),
            "https://x.com/ann_lee/status/123"
        );
        assert_eq!(status_url(None, "123"), "https://x.com/i/status/123");
        assert_eq!(status_url(Some("a/b"), "1"), "https://x.com/i/status/1");
    }

    #[test]
    fn refusals_say_what_to_do() {
        let none = Value::Null;
        // The API: a 401 is a dead token; the token endpoint: a 400 is a refused grant.
        let api = |s: u16, b: &Value| {
            refusal(
                StatusCode::from_u16(s).unwrap(),
                &headers(None),
                b,
                "Publish",
                false,
            )
        };
        let tok = |s: u16, b: &Value| {
            refusal(
                StatusCode::from_u16(s).unwrap(),
                &headers(None),
                b,
                "Refresh",
                true,
            )
        };
        assert_eq!(api(401, &none), PostError::Reconnect);
        assert_eq!(tok(400, &none), PostError::Reconnect);
        assert!(matches!(tok(401, &none), PostError::Failed(_)));
        assert!(matches!(api(400, &none), PostError::Failed(_)));

        // Credits: 402, or any status whose error speaks of credits.
        for b in [
            none.clone(),
            json!({"title": "CreditsDepleted", "detail": "No credits."}),
        ] {
            assert_eq!(
                api(402, &b),
                PostError::Failed(format!("Publish: {CREDITS}"))
            );
        }
        let credits = json!({"type": "https://api.x.com/2/problems/credits", "detail": "x"});
        assert_eq!(
            api(403, &credits),
            PostError::Failed(format!("Publish: {CREDITS}"))
        );

        // A plain 403 keeps X's reason, links cut.
        let dup = json!({"detail": "You are not allowed to create a Tweet with duplicate content. https://x.example/?access_token=a"});
        assert_eq!(
            api(403, &dup),
            PostError::Failed(
                "Publish: You are not allowed to create a Tweet with duplicate content.".into()
            )
        );
    }

    #[test]
    fn a_rate_limit_names_when_it_resets() {
        let busy = json!({"title": "Too Many Requests"});
        // 2026-11-04 12:30:00 UTC.
        let said = describe(
            StatusCode::TOO_MANY_REQUESTS,
            &headers(Some("1793795400")),
            &busy,
        );
        assert_eq!(
            said,
            "X rate limit reached; it resets at 2026-11-04 12:30 UTC. It was not retried."
        );
        assert_eq!(
            describe(StatusCode::TOO_MANY_REQUESTS, &headers(None), &busy),
            "X rate limit reached. It was not retried."
        );
        let capped = json!({"type": "https://api.x.com/2/problems/usage-capped"});
        assert_eq!(
            describe(StatusCode::TOO_MANY_REQUESTS, &headers(Some("1")), &capped),
            USAGE_CAPPED
        );
    }

    #[test]
    fn a_body_part_is_framed_with_its_boundary() {
        let (content_type, body) = multipart(("segment_index", "2"), ("media", b"\x00\x01data"));
        let boundary = content_type.split("boundary=").nth(1).unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with(&format!("--{boundary}\r\n")));
        assert!(text.contains("name=\"segment_index\"\r\n\r\n2\r\n"));
        assert!(text.contains("name=\"media\"; filename=\"chunk\""));
        assert!(text.ends_with(&format!("\r\n--{boundary}--\r\n")));
        assert!(body.windows(6).any(|w| w == b"\x00\x01data"));
    }
}
