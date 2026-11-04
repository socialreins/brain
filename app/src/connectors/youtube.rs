//! YouTube (a channel): the OAuth code swap and token refresh on the Google client, the channel
//! lookup, a resumable upload through the Data API v3, and take down. A post is one video. An
//! access token lasts an hour; the refresh token Google hands out on connect (`access_type=
//! offline`) renews it. Errors carry no URL, token or secret.
use std::time::Duration;

use reqwest::{header::HeaderMap, Method, RequestBuilder, StatusCode, Url};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{
    bluesky::PostError,
    google::{AUTH_URL, TOKEN_URL},
};

const API_URL: &str = "https://www.googleapis.com";
/// Upload, read the channel, and delete a video (which needs `youtube.force-ssl`).
const SCOPES: &str = "https://www.googleapis.com/auth/youtube.upload \
                      https://www.googleapis.com/auth/youtube.readonly \
                      https://www.googleapis.com/auth/youtube.force-ssl";
/// Used when Google leaves `expires_in` out.
const DEFAULT_EXPIRES_IN: i64 = 3600;
/// Longest one post (session, upload of the bytes) may take.
pub const PUBLISH_TIMEOUT: Duration = Duration::from_secs(480);
/// The upload of the bytes itself, longer than the other calls' timeout.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(420);
pub const VIDEO_TYPES: [&str; 2] = ["video/mp4", "video/quicktime"];
/// Until Google audits the API project, YouTube locks what it uploads to private whatever this
/// says (see README).
const PRIVACY: &str = "public";

const QUOTA: &str = "YouTube's daily quota for this app is used up (an upload costs 1,600 of the \
                     10,000 units a day). Try again after midnight Pacific time.";

/// The Google client (`GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET`), the same one as Google
/// sign-in, under its own redirect URI. No `Debug`: it holds the secret.
#[derive(Clone)]
pub struct YouTube {
    /// `(client id, client secret)`; connecting and refreshing need them, calls with a live
    /// access token do not.
    app: Option<(String, String)>,
    /// Tests point these at a mock.
    pub auth_url: String,
    pub token_url: String,
    /// Serves both `/youtube/v3` and `/upload/youtube/v3`.
    pub api_url: String,
    client: reqwest::Client,
}

pub struct Token {
    pub access_token: String,
    pub expires_in: i64,
    /// Only the code swap returns one; a refresh keeps the old.
    pub refresh_token: Option<String>,
    /// The scopes Google granted, space-separated. With granular consent the user can untick some.
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
            .split(' ')
            .collect();
        SCOPES.split(' ').all(|s| granted.contains(&s))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Channel {
    pub id: String,
    pub title: Option<String>,
    /// The `@handle`, without the `@`.
    pub handle: Option<String>,
    pub thumbnail: Option<String>,
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: String,
    expires_in: Option<i64>,
    refresh_token: Option<String>,
    scope: Option<String>,
}

/// The video to post: its bytes (shared, not copied, when sent) and type.
pub struct File<'a> {
    pub bytes: bytes::Bytes,
    pub content_type: &'a str,
}

fn failed(msg: impl Into<String>) -> PostError {
    PostError::Failed(msg.into())
}

/// Google's own words for a refused call, without links, cut short. Data API errors nest the
/// message in `error`; token endpoint errors put it in `error_description`.
fn reason(body: &Value, status: StatusCode) -> String {
    let msg = body["error"]["message"]
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

/// The first machine-readable reason of a Data API error, e.g. `quotaExceeded`.
fn code(body: &Value) -> &str {
    body["error"]["errors"][0]["reason"]
        .as_str()
        .unwrap_or_default()
}

/// What a refused call means: a dead token or a missing permission is `Reconnect`, an
/// exhausted quota says so, anything else is `Failed` with `step` and Google's reason.
fn refusal(status: StatusCode, body: &Value, step: &str) -> PostError {
    match (status.as_u16(), code(body)) {
        (401, _) | (403, "insufficientPermissions" | "authError") => PostError::Reconnect,
        // A revoked or expired refresh token, or a code that was already used.
        (400, _) if body["error"] == "invalid_grant" => PostError::Reconnect,
        (_, "quotaExceeded" | "dailyLimitExceeded") => failed(QUOTA),
        (_, "uploadLimitExceeded") => {
            failed("This channel reached YouTube's limit on uploads for now. Try again later.")
        }
        _ => failed(format!("{step}: {}", reason(body, status))),
    }
}

/// A video id is 11 URL-safe characters; anything else never reaches a URL.
pub fn video_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// The public link to a video.
pub fn watch_url(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

impl YouTube {
    pub fn new(app: Option<(String, String)>) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("SocialReins")
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        Ok(Self {
            app,
            auth_url: AUTH_URL.into(),
            token_url: TOKEN_URL.into(),
            api_url: API_URL.into(),
            client,
        })
    }

    /// Whether the client id and secret are set, so channels can be connected.
    pub fn configured(&self) -> bool {
        self.app.is_some()
    }

    /// Where `GET /connections/youtube/redirect` sends the browser; `None` when not configured.
    /// `prompt=consent` makes Google return a refresh token every time, and
    /// `include_granted_scopes=false` keeps the sign-in scopes out of this grant.
    pub fn authorize_url(&self, redirect_uri: &str, state: &str) -> Option<String> {
        let (id, _) = self.app.as_ref()?;
        let params = [
            ("response_type", "code"),
            ("client_id", id.as_str()),
            ("redirect_uri", redirect_uri),
            ("state", state),
            ("scope", SCOPES),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("include_granted_scopes", "false"),
        ];
        Url::parse_with_params(&self.auth_url, params)
            .ok()
            .map(String::from)
    }

    /// Sends a call and gives its status, headers and JSON body. Only a call that never got an
    /// answer is an error.
    async fn raw(
        &self,
        req: RequestBuilder,
        step: &str,
    ) -> Result<(StatusCode, HeaderMap, Value), PostError> {
        let res = req.send().await.map_err(|e| {
            tracing::warn!(error = %e.without_url(), step, "youtube call failed");
            failed(format!("{step}: YouTube could not be reached."))
        })?;
        let status = res.status();
        let headers = res.headers().clone();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        Ok((status, headers, body))
    }

    /// `raw`, with anything not 2xx turned into its `refusal`.
    async fn send(&self, req: RequestBuilder, step: &str) -> Result<(HeaderMap, Value), PostError> {
        let (status, headers, body) = self.raw(req, step).await?;
        if status.is_success() {
            Ok((headers, body))
        } else {
            Err(refusal(status, &body, step))
        }
    }

    async fn token(&self, form: &[(&str, &str)], step: &str) -> Result<Token, PostError> {
        let Some((id, secret)) = &self.app else {
            return Err(failed("YouTube is not set up."));
        };
        let mut form = form.to_vec();
        form.extend([
            ("client_id", id.as_str()),
            ("client_secret", secret.as_str()),
        ]);
        let req = self.client.post(&self.token_url).form(&form);
        let (_, body) = self.send(req, step).await?;
        let t: TokenAnswer = serde_json::from_value(body)
            .map_err(|_| failed(format!("{step}: Google sent no token.")))?;
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

    /// Swaps an authorization code for a token.
    pub async fn exchange(&self, code: &str, redirect_uri: &str) -> Result<Token, PostError> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
        ];
        self.token(&form, "Code").await
    }

    /// A new access token from the refresh token. A refused one is `Reconnect`.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Token, PostError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ];
        self.token(&form, "Refresh").await
    }

    /// The channel the consent picked, or `None` when the Google account has none.
    pub async fn channel(&self, access_token: &str) -> Result<Option<Channel>, PostError> {
        let url = format!(
            "{}/youtube/v3/channels?part=snippet&mine=true",
            self.api_url
        );
        let req = self.client.get(url).bearer_auth(access_token);
        let (_, body) = self.send(req, "Channel").await?;
        let Some(item) = body["items"].get(0) else {
            return Ok(None);
        };
        let Some(id) = item["id"].as_str().filter(|i| !i.is_empty()) else {
            return Err(failed("Channel: YouTube did not say which channel."));
        };
        let snippet = &item["snippet"];
        let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        let thumbs = &snippet["thumbnails"];
        Ok(Some(Channel {
            id: id.to_owned(),
            title: text(&snippet["title"]),
            handle: text(&snippet["customUrl"])
                .map(|h| h.trim_start_matches('@').to_owned())
                .filter(|h| !h.is_empty()),
            thumbnail: text(&thumbs["medium"]["url"]).or_else(|| text(&thumbs["default"]["url"])),
        }))
    }

    /// Whether an upload session URL Google handed out is Google's own: https on `googleapis.com`
    /// or a subdomain, or exactly the API's origin (scheme, host and port; a mock in tests). The
    /// URL is parsed, so lookalikes (`www.googleapis.com.evil.net`) and userinfo tricks
    /// (`https://www.googleapis.com@evil.net`) fail.
    fn upload_allowed(&self, url: &str) -> bool {
        let Ok(u) = Url::parse(url) else {
            return false;
        };
        if !u.username().is_empty() || u.password().is_some() {
            return false;
        }
        let host = u.host_str().unwrap_or_default();
        let google = u.scheme() == "https"
            && u.port_or_known_default() == Some(443)
            && (host == "googleapis.com" || host.ends_with(".googleapis.com"));
        let api = Url::parse(&self.api_url).is_ok_and(|a| a.origin() == u.origin());
        google || api
    }

    /// Uploads `video` as a public video titled `title` and returns its id. A resumable
    /// session (`uploadType=resumable`), then one `PUT` of all the bytes; never resumed or
    /// retried, since a retry could post twice. The token goes along only to a session URL that
    /// passed `upload_allowed`.
    pub async fn upload(
        &self,
        token: &str,
        (title, description): (&str, &str),
        video: &File<'_>,
    ) -> Result<String, PostError> {
        let meta = json!({
            "snippet": {"title": title, "description": description},
            "status": {"privacyStatus": PRIVACY, "selfDeclaredMadeForKids": false},
        });
        let start = format!(
            "{}/upload/youtube/v3/videos?uploadType=resumable&part=snippet,status",
            self.api_url
        );
        let req = self
            .client
            .request(Method::POST, start)
            .bearer_auth(token)
            .header("X-Upload-Content-Length", video.bytes.len())
            .header("X-Upload-Content-Type", video.content_type)
            .json(&meta);
        let (headers, _) = self.send(req, "Upload").await?;
        let session = headers
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| failed("Upload: YouTube sent no upload URL."))?;
        if !self.upload_allowed(session) {
            return Err(failed("Upload: YouTube sent an unexpected upload URL."));
        }
        let req = self
            .client
            .put(session)
            .bearer_auth(token)
            .header("Content-Type", video.content_type)
            .timeout(UPLOAD_TIMEOUT)
            .body(video.bytes.clone());
        let (_, body) = self.send(req, "Upload").await?;
        body["id"]
            .as_str()
            .filter(|id| video_id(id))
            .map(str::to_owned)
            .ok_or_else(|| {
                failed("Upload: YouTube sent no video id. It may have posted, check the channel.")
            })
    }

    /// Takes a video down. A video already gone counts as down.
    pub async fn delete_video(&self, token: &str, id: &str) -> Result<(), PostError> {
        if !video_id(id) {
            return Err(failed("This is not a YouTube video id."));
        }
        let url =
            Url::parse_with_params(&format!("{}/youtube/v3/videos", self.api_url), [("id", id)])
                .map_err(|_| failed("This is not a YouTube video id."))?;
        let req = self.client.delete(url).bearer_auth(token);
        let (status, _, body) = self.raw(req, "Delete").await?;
        if status.is_success() || code(&body) == "videoNotFound" {
            Ok(())
        } else {
            Err(refusal(status, &body, "Delete"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authorize_url_asks_for_offline_access_and_the_youtube_scopes() {
        let y = YouTube::new(Some(("app".into(), "secret".into()))).unwrap();
        let url = y
            .authorize_url("https://api.example/connect/youtube/callback", "st")
            .unwrap();
        let parsed = Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(parsed.host_str(), Some("accounts.google.com"));
        assert_eq!(q["client_id"], "app");
        assert_eq!(q["state"], "st");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["access_type"], "offline");
        assert_eq!(q["prompt"], "consent");
        assert_eq!(q["include_granted_scopes"], "false");
        let scopes: Vec<&str> = q["scope"].split(' ').collect();
        assert_eq!(
            scopes,
            [
                "https://www.googleapis.com/auth/youtube.upload",
                "https://www.googleapis.com/auth/youtube.readonly",
                "https://www.googleapis.com/auth/youtube.force-ssl",
            ]
        );
        assert!(!url.contains("secret"));
        assert!(YouTube::new(None)
            .unwrap()
            .authorize_url("https://x", "st")
            .is_none());
    }

    #[test]
    fn only_googles_own_session_urls_are_accepted() {
        let y = YouTube::new(None).unwrap();
        for ok in [
            "https://www.googleapis.com/upload/youtube/v3/videos?uploadType=resumable&upload_id=x",
            "https://upload.googleapis.com/x",
            "https://googleapis.com/x",
        ] {
            assert!(y.upload_allowed(ok), "{ok}");
        }
        for bad in [
            "https://www.googleapis.com.evil.net/x",
            "https://www.googleapis.com@evil.net/x",
            "https://user:pw@www.googleapis.com/x",
            "https://www.googleapis.com:8443/x",
            "https://evil.net/www.googleapis.com",
            "https://notgoogleapis.com/x",
            "http://www.googleapis.com/x",
            "//evil.net/x",
            "not a url",
        ] {
            assert!(!y.upload_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn refusals_say_what_to_do() {
        let err = |status: u16, body: Value| {
            refusal(StatusCode::from_u16(status).unwrap(), &body, "Upload")
        };
        let nested = |reason: &str| {
            json!({"error": {"code": 403, "message": "No.",
            "errors": [{"reason": reason}]}})
        };
        assert!(matches!(err(401, Value::Null), PostError::Reconnect));
        assert!(matches!(
            err(403, nested("insufficientPermissions")),
            PostError::Reconnect
        ));
        assert!(matches!(
            err(
                400,
                json!({"error": "invalid_grant", "error_description": "Bad."})
            ),
            PostError::Reconnect
        ));
        match err(403, nested("quotaExceeded")) {
            PostError::Failed(m) => assert!(m.contains("daily quota"), "{m}"),
            PostError::Reconnect => panic!("quota is not a reconnect"),
        }
        match err(
            400,
            json!({"error": {"message": "Bad https://x.example/?access_token=a title"}}),
        ) {
            PostError::Failed(m) => assert_eq!(m, "Upload: Bad title"),
            PostError::Reconnect => panic!(),
        }
        match err(502, Value::Null) {
            PostError::Failed(m) => assert_eq!(m, "Upload: HTTP 502"),
            PostError::Reconnect => panic!(),
        }
    }

    #[test]
    fn a_token_needs_every_scope_asked_for() {
        let token = |scope: Option<&str>| Token {
            access_token: "t".into(),
            expires_in: 3600,
            refresh_token: None,
            scope: scope.map(str::to_owned),
        };
        assert!(token(Some(SCOPES)).has_all_scopes());
        // Order and extra scopes do not matter.
        let shuffled = "openid https://www.googleapis.com/auth/youtube.force-ssl \
                        https://www.googleapis.com/auth/youtube.upload \
                        https://www.googleapis.com/auth/youtube.readonly";
        assert!(token(Some(shuffled)).has_all_scopes());
        for missing in SCOPES.split(' ') {
            let rest: Vec<&str> = SCOPES.split(' ').filter(|s| *s != missing).collect();
            assert!(!token(Some(&rest.join(" "))).has_all_scopes(), "{missing}");
        }
        assert!(!token(Some("")).has_all_scopes());
        assert!(!token(None).has_all_scopes());
    }

    #[test]
    fn video_ids_are_plain() {
        assert!(video_id("dQw4w9WgXcQ"));
        assert!(!video_id(""));
        assert!(!video_id("a/b"));
        assert!(!video_id("a&id=b"));
        assert_eq!(watch_url("abc"), "https://www.youtube.com/watch?v=abc");
    }
}
