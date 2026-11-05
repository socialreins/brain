//! LinkedIn (a member's own profile): the OAuth code swap, the OpenID profile, publishing with
//! the Posts, Images and Videos APIs, and take down. A post is text, one or several images, or
//! one video; media is uploaded as bytes first. Tokens last about 60 days; only partner apps get
//! a refresh token. Errors carry no URL, token or secret.
use std::time::Duration;

use reqwest::{header::HeaderMap, Method, RequestBuilder, Url};
use serde::Deserialize;
use serde_json::{json, Value};

use super::bluesky::PostError;

const AUTH_URL: &str = "https://www.linkedin.com/oauth/v2/authorization";
const OAUTH_URL: &str = "https://www.linkedin.com/oauth/v2/accessToken";
const API_URL: &str = "https://api.linkedin.com";
/// The Marketing API version every `/rest` call pins. A version lives about a year: bump this
/// before it sunsets.
pub const API_VERSION: &str = "202610";
const SCOPES: &str = "openid profile email w_member_social";
/// Used when LinkedIn leaves `expires_in` out.
const DEFAULT_EXPIRES_IN: i64 = 60 * 24 * 3600;
/// Video status checks: now, then after 1, 2, ... `POLLS - 1` times `poll_every`.
const POLLS: u32 = 10;
/// Longest one post (uploads, video processing and the publish) may take.
pub const PUBLISH_TIMEOUT: Duration = Duration::from_secs(420);
/// Images a post may hold, and what LinkedIn takes.
pub const MAX_IMAGES: usize = 20;
pub const IMAGE_TYPES: [&str; 3] = ["image/jpeg", "image/png", "image/gif"];
pub const VIDEO_TYPE: &str = "video/mp4";

/// The LinkedIn app (`LINKEDIN_CLIENT_ID`, `LINKEDIN_CLIENT_SECRET`). No `Debug`: it holds the
/// secret.
#[derive(Clone)]
pub struct LinkedIn {
    /// `(client id, client secret)`; connecting needs them, publishing with a stored token does not.
    app: Option<(String, String)>,
    /// Tests point these at a mock, and shorten `poll_every`.
    pub auth_url: String,
    pub oauth_url: String,
    pub api_url: String,
    pub poll_every: Duration,
    client: reqwest::Client,
}

pub struct Token {
    pub access_token: String,
    pub expires_in: i64,
    /// Only partner apps get one.
    pub refresh_token: Option<String>,
}

#[derive(Deserialize)]
pub struct Profile {
    pub sub: String,
    pub name: Option<String>,
    pub picture: Option<String>,
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: String,
    expires_in: Option<i64>,
    refresh_token: Option<String>,
}

/// One file to post: its bytes and type.
pub struct File<'a> {
    pub bytes: &'a [u8],
    pub content_type: &'a str,
}

fn failed(msg: impl Into<String>) -> PostError {
    PostError::Failed(msg.into())
}

/// LinkedIn's own words for a refused call, without links, cut short.
fn reason(body: &Value, status: reqwest::StatusCode) -> String {
    let msg = body["message"]
        .as_str()
        .or(body["error_description"].as_str())
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

/// `urn:li:share:123` as one path segment. Only plain URNs pass.
fn encoded(urn: &str) -> Option<String> {
    let plain = urn.starts_with("urn:li:")
        && urn.len() <= 200
        && urn
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-'));
    plain.then(|| urn.replace(':', "%3A"))
}

/// The public link to a post.
pub fn post_url(urn: &str) -> String {
    format!("https://www.linkedin.com/feed/update/{urn}")
}

/// Escapes what LinkedIn's "little" text format reserves, so the body posts as written. A `#`
/// that starts a word is left alone, so hashtags still link.
pub fn commentary(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let reserved = match c {
            '\\' | '|' | '{' | '}' | '@' | '[' | ']' | '(' | ')' | '<' | '>' | '*' | '_' | '~' => {
                true
            }
            '#' => !chars
                .peek()
                .is_some_and(|n| n.is_alphanumeric() || *n == '_'),
            _ => false,
        };
        if reserved {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

impl LinkedIn {
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
            oauth_url: OAUTH_URL.into(),
            api_url: API_URL.into(),
            poll_every: Duration::from_secs(5),
            client,
        })
    }

    /// Whether the client id and secret are set, so accounts can be connected.
    pub fn configured(&self) -> bool {
        self.app.is_some()
    }

    /// Where `GET /connections/linkedin/redirect` sends the browser; `None` when not configured.
    pub fn authorize_url(&self, redirect_uri: &str, state: &str) -> Option<String> {
        let (id, _) = self.app.as_ref()?;
        let params = [
            ("response_type", "code"),
            ("client_id", id.as_str()),
            ("redirect_uri", redirect_uri),
            ("state", state),
            ("scope", SCOPES),
        ];
        Url::parse_with_params(&self.auth_url, params)
            .ok()
            .map(String::from)
    }

    /// Sends a call: its headers and JSON body. A refused token or permission is `Reconnect`,
    /// anything else not 2xx is `Failed` with `step` and LinkedIn's reason.
    async fn send(&self, req: RequestBuilder, step: &str) -> Result<(HeaderMap, Value), PostError> {
        let res = req.send().await.map_err(|e| {
            tracing::warn!(error = %e.without_url(), step, "linkedin call failed");
            failed(format!("{step}: LinkedIn could not be reached."))
        })?;
        let status = res.status();
        let headers = res.headers().clone();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok((headers, body));
        }
        if matches!(status.as_u16(), 401 | 403) {
            return Err(PostError::Reconnect);
        }
        Err(failed(format!("{step}: {}", reason(&body, status))))
    }

    /// A `/rest` call, pinned to `API_VERSION`.
    fn rest(&self, method: Method, path: &str, token: &str) -> RequestBuilder {
        self.client
            .request(method, format!("{}/rest/{path}", self.api_url))
            .bearer_auth(token)
            .header("LinkedIn-Version", API_VERSION)
            .header("X-Restli-Protocol-Version", "2.0.0")
    }

    async fn token(&self, form: &[(&str, &str)], step: &str) -> Result<Token, PostError> {
        let Some((id, secret)) = &self.app else {
            return Err(failed("LinkedIn is not set up."));
        };
        let mut form = form.to_vec();
        form.extend([
            ("client_id", id.as_str()),
            ("client_secret", secret.as_str()),
        ]);
        let req = self.client.post(&self.oauth_url).form(&form);
        let (_, body) = self.send(req, step).await?;
        let t: TokenAnswer = serde_json::from_value(body)
            .map_err(|_| failed(format!("{step}: LinkedIn sent no token.")))?;
        Ok(Token {
            access_token: t.access_token,
            expires_in: t
                .expires_in
                .filter(|s| *s > 0)
                .unwrap_or(DEFAULT_EXPIRES_IN),
            refresh_token: t.refresh_token.filter(|r| !r.is_empty()),
        })
    }

    /// Swaps an authorization code for a token.
    pub async fn exchange(&self, code: &str, redirect_uri: &str) -> Result<Token, PostError> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
        ];
        self.token(&form, "Code").await
    }

    pub async fn refresh(&self, refresh_token: &str) -> Result<Token, PostError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        self.token(&form, "Refresh").await
    }

    /// The signed-in member's OpenID profile.
    pub async fn me(&self, access_token: &str) -> Result<Profile, PostError> {
        let req = self
            .client
            .get(format!("{}/v2/userinfo", self.api_url))
            .bearer_auth(access_token);
        let (_, body) = self.send(req, "Profile").await?;
        let me: Profile = serde_json::from_value(body)
            .map_err(|_| failed("Profile: LinkedIn did not say who signed in."))?;
        if me.sub.is_empty() {
            return Err(failed("Profile: LinkedIn did not say who signed in."));
        }
        Ok(me)
    }

    /// Posts as the member `sub`: text, one or several images, or one video. Returns the post's
    /// URN.
    pub async fn publish_post(
        &self,
        sub: &str,
        token: &str,
        text: &str,
        media: &[File<'_>],
    ) -> Result<String, PostError> {
        let author = format!("urn:li:person:{sub}");
        let videos = media
            .iter()
            .filter(|m| m.content_type == VIDEO_TYPE)
            .count();
        if videos > 0 && media.len() > 1 {
            return Err(failed(
                "LinkedIn takes one video, or images, but not both or several videos.",
            ));
        }
        let content = if videos == 1 {
            let id = self.upload_video(&author, token, media[0].bytes).await?;
            Some(json!({"media": {"id": id}}))
        } else if media.is_empty() {
            None
        } else {
            let mut ids = Vec::new();
            for m in media {
                ids.push(self.upload_image(&author, token, m).await?);
            }
            if let [one] = ids.as_slice() {
                Some(json!({"media": {"id": one}}))
            } else {
                let images: Vec<Value> = ids.iter().map(|id| json!({"id": id})).collect();
                Some(json!({"multiImage": {"images": images}}))
            }
        };
        let mut body = json!({
            "author": author,
            "commentary": commentary(text),
            "visibility": "PUBLIC",
            "distribution": {
                "feedDistribution": "MAIN_FEED",
                "targetEntities": [],
                "thirdPartyDistributionChannels": [],
            },
            "lifecycleState": "PUBLISHED",
            "isReshareDisabledByAuthor": false,
        });
        if let Some(c) = content {
            body["content"] = c;
        }
        let req = self.rest(Method::POST, "posts", token).json(&body);
        let (headers, _) = self.send(req, "Publish").await?;
        headers
            .get("x-restli-id")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| failed("Publish: LinkedIn sent no post id."))
    }

    /// `initializeUpload` of `kind` (`images` or `videos`): the `value` object.
    async fn initialize(
        &self,
        kind: &str,
        token: &str,
        request: Value,
    ) -> Result<Value, PostError> {
        let path = format!("{kind}?action=initializeUpload");
        let req = self.rest(Method::POST, &path, token).json(&request);
        let (_, body) = self.send(req, "Upload").await?;
        Ok(body["value"].clone())
    }

    /// Whether an upload URL LinkedIn handed out is LinkedIn's own: https on `linkedin.com` or a
    /// subdomain, or exactly the API's origin (scheme, host and port; a mock in tests). The URL is
    /// parsed, so lookalikes (`api.linkedin.com.evil.net`) and userinfo tricks
    /// (`https://api.linkedin.com@evil.net`) fail.
    fn upload_allowed(&self, url: &str) -> bool {
        let Ok(u) = Url::parse(url) else {
            return false;
        };
        if !u.username().is_empty() || u.password().is_some() {
            return false;
        }
        let host = u.host_str().unwrap_or_default();
        let linkedin =
            u.scheme() == "https" && (host == "linkedin.com" || host.ends_with(".linkedin.com"));
        let api = Url::parse(&self.api_url).is_ok_and(|a| a.origin() == u.origin());
        linkedin || api
    }

    /// PUTs `bytes` to a URL LinkedIn handed out (see `upload_allowed`); returns the answer's
    /// `ETag`. The token goes along only to such a URL.
    async fn put(
        &self,
        url: &str,
        token: Option<&str>,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<String, PostError> {
        if !self.upload_allowed(url) {
            return Err(failed("Upload: LinkedIn sent an unexpected upload URL."));
        }
        let mut req = self
            .client
            .put(url)
            .header("Content-Type", content_type)
            .body(bytes.to_vec());
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let (headers, _) = self.send(req, "Upload").await?;
        Ok(headers
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.trim_matches('"').to_owned())
            .unwrap_or_default())
    }

    async fn upload_image(
        &self,
        owner: &str,
        token: &str,
        file: &File<'_>,
    ) -> Result<String, PostError> {
        let request = json!({"initializeUploadRequest": {"owner": owner}});
        let value = self.initialize("images", token, request).await?;
        let (Some(url), Some(image)) = (value["uploadUrl"].as_str(), value["image"].as_str())
        else {
            return Err(failed("Upload: LinkedIn sent no image upload URL."));
        };
        self.put(url, Some(token), file.bytes, file.content_type)
            .await?;
        Ok(image.to_owned())
    }

    /// Uploads a video in the parts LinkedIn asks for, finalizes it and waits until LinkedIn has
    /// processed it.
    async fn upload_video(
        &self,
        owner: &str,
        token: &str,
        bytes: &[u8],
    ) -> Result<String, PostError> {
        let request = json!({"initializeUploadRequest": {
            "owner": owner, "fileSizeBytes": bytes.len(),
            "uploadCaptions": false, "uploadThumbnail": false}});
        let value = self.initialize("videos", token, request).await?;
        let (Some(video), Some(parts)) = (
            value["video"].as_str(),
            value["uploadInstructions"].as_array(),
        ) else {
            return Err(failed("Upload: LinkedIn sent no video upload URL."));
        };
        let mut etags = Vec::new();
        for part in parts {
            let first = part["firstByte"].as_u64().unwrap_or(0) as usize;
            let last = part["lastByte"].as_u64().unwrap_or(0) as usize;
            let (Some(url), Some(chunk)) = (
                part["uploadUrl"].as_str(),
                bytes.get(first..=last.min(bytes.len().saturating_sub(1))),
            ) else {
                return Err(failed("Upload: LinkedIn sent a bad video upload plan."));
            };
            // The docs upload video parts without the token.
            etags.push(
                self.put(url, None, chunk, "application/octet-stream")
                    .await?,
            );
        }
        let finalize = json!({"finalizeUploadRequest": {
            "video": video,
            "uploadToken": value["uploadToken"].as_str().unwrap_or_default(),
            "uploadedPartIds": etags}});
        let req = self
            .rest(Method::POST, "videos?action=finalizeUpload", token)
            .json(&finalize);
        self.send(req, "Upload").await?;
        self.video_ready(video, token).await?;
        Ok(video.to_owned())
    }

    async fn video_ready(&self, video: &str, token: &str) -> Result<(), PostError> {
        let path = format!(
            "videos/{}",
            encoded(video).ok_or_else(|| failed("Upload: bad video id."))?
        );
        for i in 1..=POLLS {
            let req = self.rest(Method::GET, &path, token);
            let (_, body) = self.send(req, "Status").await?;
            match body["status"].as_str() {
                Some("AVAILABLE") => return Ok(()),
                Some("PROCESSING_FAILED") => {
                    let why = body["processingFailureReason"].as_str().unwrap_or("failed");
                    let why: String = why.chars().take(200).collect();
                    return Err(failed(format!(
                        "LinkedIn could not process the video: {why}"
                    )));
                }
                _ => {}
            }
            if i < POLLS {
                tokio::time::sleep(self.poll_every * i).await;
            }
        }
        Err(failed(
            "LinkedIn did not finish processing the video in time.",
        ))
    }

    /// Comments `text` on the post `urn` as the member `sub` (`POST /rest/socialActions/{urn}/
    /// comments`), plain text with no escaping. Returns the comment's id. LinkedIn's docs name
    /// `w_member_social_feed` for this call, not the `w_member_social` this connector asks for, so
    /// a refusal says so instead of asking for a reconnect that cannot help. Once, never retried.
    pub async fn comment_post(
        &self,
        sub: &str,
        token: &str,
        urn: &str,
        text: &str,
    ) -> Result<String, PostError> {
        let Some(path) = encoded(urn).map(|e| format!("socialActions/{e}/comments")) else {
            return Err(failed("This is not a LinkedIn post id."));
        };
        let body = json!({
            "actor": format!("urn:li:person:{sub}"),
            "object": urn,
            "message": {"text": text},
        });
        let req = self.rest(Method::POST, &path, token).json(&body);
        let (headers, body) = match self.send(req, "Comment").await {
            Ok(done) => done,
            Err(PostError::Reconnect) => {
                return Err(failed(
                    "Comment: LinkedIn refused it. The app may lack the w_member_social_feed \
                     permission, or the token expired; reconnect the account if it did.",
                ))
            }
            Err(e) => return Err(e),
        };
        headers
            .get("x-restli-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .or_else(|| body["id"].as_str().map(str::to_owned))
            .filter(|v| !v.is_empty())
            .ok_or_else(|| failed("Comment: LinkedIn sent no comment id."))
    }

    /// Takes a post down (`w_member_social`). LinkedIn answers 204 also for one already gone.
    pub async fn delete_post(&self, token: &str, urn: &str) -> Result<(), PostError> {
        let Some(path) = encoded(urn).map(|e| format!("posts/{e}")) else {
            return Err(failed("This is not a LinkedIn post id."));
        };
        let req = self
            .rest(Method::DELETE, &path, token)
            .header("X-RestLi-Method", "DELETE");
        self.send(req, "Delete").await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authorize_url_carries_the_scopes_and_state() {
        let l = LinkedIn::new(Some(("app".into(), "secret".into()))).unwrap();
        let url = l
            .authorize_url("https://api.example/connect/linkedin/callback", "st")
            .unwrap();
        let parsed = Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(parsed.host_str(), Some("www.linkedin.com"));
        assert_eq!(q["client_id"], "app");
        assert_eq!(q["scope"], "openid profile email w_member_social");
        assert_eq!(q["state"], "st");
        assert_eq!(q["response_type"], "code");
        assert!(!url.contains("secret"));
        assert!(LinkedIn::new(None)
            .unwrap()
            .authorize_url("https://x", "st")
            .is_none());
    }

    #[test]
    fn commentary_escapes_reserved_characters_but_keeps_hashtags() {
        assert_eq!(
            commentary("a (b) *c* #tag # x_y @z"),
            r"a \(b\) \*c\* #tag \# x\_y \@z"
        );
        assert_eq!(commentary("end #"), r"end \#");
        assert_eq!(commentary(r"a\b|c"), r"a\\b\|c");
        assert_eq!(commentary("plain, text."), "plain, text.");
    }

    #[test]
    fn only_linkedins_own_upload_urls_are_accepted() {
        let l = LinkedIn::new(None).unwrap();
        for ok in [
            "https://www.linkedin.com/dms-uploads/C5/uploadedVideo?sau=x",
            "https://api.linkedin.com/up/1",
            "https://linkedin.com/up",
        ] {
            assert!(l.upload_allowed(ok), "{ok}");
        }
        for bad in [
            "https://api.linkedin.com.evil.net/x",
            "https://api.linkedin.com@evil.net/x",
            "https://user:pw@www.linkedin.com/x",
            "https://evil.net/api.linkedin.com",
            "https://notlinkedin.com/x",
            "http://www.linkedin.com/x",
            "http://api.linkedin.com/x",
            "//evil.net/x",
            "not a url",
        ] {
            assert!(!l.upload_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn urns_are_encoded_only_when_plain() {
        assert_eq!(
            encoded("urn:li:share:123").as_deref(),
            Some("urn%3Ali%3Ashare%3A123")
        );
        assert_eq!(encoded("123"), None);
        assert_eq!(encoded("urn:li:share:1/../x"), None);
        assert_eq!(
            post_url("urn:li:share:1"),
            "https://www.linkedin.com/feed/update/urn:li:share:1"
        );
    }

    #[test]
    fn refusals_name_the_reason_without_links() {
        let body = json!({"message": "Bad https://api.linkedin.com/x?access_token=abc thing"});
        assert_eq!(reason(&body, reqwest::StatusCode::BAD_REQUEST), "Bad thing");
        assert_eq!(
            reason(&Value::Null, reqwest::StatusCode::BAD_GATEWAY),
            "HTTP 502"
        );
    }
}
