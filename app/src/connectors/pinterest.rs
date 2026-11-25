//! Pinterest (an account): the OAuth v5 code swap and token refresh, the profile and board
//! lookups, and creating a pin (one image, a carousel of 2 to 5 images, or one video) on a
//! board, or deleting one. Images are pulled by Pinterest from brain's media links; a video is
//! uploaded to the address Pinterest names, then polled until it is processed. An access token
//! lasts 30 days and a refresh token 60 days, renewed with every refresh. Errors carry no URL,
//! token or secret.
use std::time::Duration;

use reqwest::{multipart, RequestBuilder, StatusCode, Url};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use utoipa::ToSchema;

use super::bluesky::PostError;
use crate::media::Blob;

const AUTH_URL: &str = "https://www.pinterest.com/oauth/";
const API_URL: &str = "https://api.pinterest.com/v5";
/// Asked for at connect, joined with commas. Pinterest lists `boards:write` beside the others for
/// creating a pin; brain never creates or changes a board.
const SCOPES: &str = "boards:read,boards:write,pins:read,pins:write,user_accounts:read";
/// What the account must have granted: the rest of `SCOPES` is for Pinterest's own check.
const NEEDED: [&str; 4] = [
    "boards:read",
    "pins:read",
    "pins:write",
    "user_accounts:read",
];
/// Used when Pinterest leaves `expires_in` out: 30 days.
const DEFAULT_EXPIRES_IN: i64 = 2_592_000;
/// Longest one pin (the upload, Pinterest processing the video, creating the pin) may take.
pub const PUBLISH_TIMEOUT: Duration = Duration::from_secs(300);
/// How long to wait for Pinterest to process an uploaded video before giving up.
const POLL_FOR: Duration = Duration::from_secs(240);
/// Boards listed for the editor: 250 a page, at most 4 pages.
const BOARD_PAGES: usize = 4;
pub const IMAGE_TYPES: [&str; 2] = ["image/jpeg", "image/png"];
pub const VIDEO_TYPES: [&str; 2] = ["video/mp4", "video/quicktime"];

/// What the user chose for one pin. The board is `board_id`, else the account's default board
/// (`PUT /connections/{id}/pinterest`). The description is the post's body.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Options {
    /// The board's id, one of the account's (`GET /connections/{id}/pinterest/boards`).
    pub board_id: Option<String>,
    /// The pin's title, at most 100 characters.
    pub title: Option<String>,
    /// Where the pin leads, an http or https address of at most 2048 characters.
    pub link: Option<String>,
}

impl Options {
    /// The title and link as Pinterest takes them: blank is none.
    fn text(v: Option<&String>) -> Option<&str> {
        v.map(|s| s.trim()).filter(|s| !s.is_empty())
    }
}

/// One of an account's boards.
#[derive(Debug, PartialEq, Eq, Serialize, ToSchema)]
pub struct Board {
    pub id: String,
    pub name: String,
    /// `PUBLIC`, `PROTECTED` or `SECRET`.
    pub privacy: Option<String>,
}

/// What a pin is made of.
pub enum Media<'a> {
    Image(&'a str),
    /// 2 to 5 image links.
    Carousel(Vec<&'a str>),
    /// A stored video and its type.
    Video(&'a Blob, &'a str),
}

/// What a pin says besides its media.
pub struct Pin<'a> {
    pub board_id: &'a str,
    pub title: Option<&'a str>,
    pub description: &'a str,
    pub link: Option<&'a str>,
    pub alt: Option<&'a str>,
}

/// The Pinterest app (`PINTEREST_APP_ID`, `PINTEREST_APP_SECRET`). No `Debug`: it holds the secret.
#[derive(Clone)]
pub struct Pinterest {
    /// `(app id, app secret)`; connecting and refreshing need them, calls with a live access
    /// token do not.
    app: Option<(String, String)>,
    /// Tests point these at a mock.
    pub auth_url: String,
    pub api_url: String,
    /// How often an uploaded video is asked about; tests shorten it.
    pub poll_every: Duration,
    client: reqwest::Client,
}

pub struct Token {
    pub access_token: String,
    pub expires_in: i64,
    /// A refresh may bring the same one again, or a new one.
    pub refresh_token: Option<String>,
    /// The scopes Pinterest granted, comma or space separated.
    pub scope: Option<String>,
}

impl Token {
    /// Whether every scope posting needs was granted. A token that names none counts as missing
    /// them, so the connect fails rather than breaking at the first publish.
    pub fn has_all_scopes(&self) -> bool {
        let granted: Vec<&str> = self
            .scope
            .as_deref()
            .unwrap_or_default()
            .split([',', ' '])
            .collect();
        NEEDED.iter().all(|s| granted.contains(s))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Profile {
    /// Pinterest's id for the account, else its username.
    pub id: String,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: Option<String>,
    expires_in: Option<i64>,
    refresh_token: Option<String>,
    scope: Option<String>,
}

fn failed(msg: impl Into<String>) -> PostError {
    PostError::Failed(msg.into())
}

/// Pinterest's own words for a refused call (`message`), without links, cut short.
fn reason(body: &Value, status: StatusCode) -> String {
    let msg = body["message"].as_str().unwrap_or_default();
    let words: Vec<&str> = msg
        .split_whitespace()
        .filter(|w| !w.contains("://"))
        .collect();
    let msg: String = words.join(" ").chars().take(200).collect();
    if msg.is_empty() {
        format!("HTTP {}", status.as_u16())
    } else {
        msg
    }
}

/// What a refused call means: a dead token is `Reconnect`, a missing scope `Denied`, a rate limit
/// says so (nothing was posted), anything else is `Failed` with `step` and Pinterest's reason.
fn refusal(status: StatusCode, body: &Value, step: &str) -> PostError {
    let msg = reason(body, status);
    match status.as_u16() {
        401 => PostError::Reconnect,
        403 if msg.to_ascii_lowercase().contains("scope") => PostError::Denied,
        429 => failed(super::unsent(
            "Pinterest is limiting this account or SocialReins for now. Try again in a little while."
                .into(),
            true,
        )),
        _ => failed(format!("{step}: {msg}")),
    }
}

/// The public link to a pin, from its numeric id.
pub fn pin_url(id: &str) -> Option<String> {
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        .then(|| format!("https://www.pinterest.com/pin/{id}/"))
}

impl Pinterest {
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
            api_url: API_URL.into(),
            poll_every: Duration::from_secs(3),
            client,
        })
    }

    /// Whether the app id and secret are set, so accounts can be connected.
    pub fn configured(&self) -> bool {
        self.app.is_some()
    }

    /// Where `GET /connections/pinterest/redirect` sends the browser; `None` when not configured.
    pub fn authorize_url(&self, redirect_uri: &str, state: &str) -> Option<String> {
        let (id, _) = self.app.as_ref()?;
        let params = [
            ("client_id", id.as_str()),
            ("redirect_uri", redirect_uri),
            ("response_type", "code"),
            ("scope", SCOPES),
            ("state", state),
        ];
        Url::parse_with_params(&self.auth_url, params)
            .ok()
            .map(String::from)
    }

    /// Sends a call and gives its status and JSON body. Only a call that never got an answer is
    /// an error.
    async fn raw(&self, req: RequestBuilder, step: &str) -> Result<(StatusCode, Value), PostError> {
        let res = req.send().await.map_err(|e| {
            let sure = super::never_sent(&e);
            tracing::warn!(error = %e.without_url(), step, "pinterest call failed");
            let msg = format!("{step}: Pinterest could not be reached.");
            failed(super::unsent(msg, sure))
        })?;
        let status = res.status();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        Ok((status, body))
    }

    /// `raw`, with anything not 2xx turned into its `refusal`; the body of the answer.
    async fn send(&self, req: RequestBuilder, step: &str) -> Result<Value, PostError> {
        let (status, body) = self.raw(req, step).await?;
        if status.is_success() {
            return Ok(body);
        }
        tracing::warn!(
            step,
            status = status.as_u16(),
            code = body["code"].as_i64().unwrap_or_default(),
            "pinterest refused a call"
        );
        Err(refusal(status, &body, step))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.api_url)
    }

    async fn token(&self, form: &[(&str, &str)], step: &str) -> Result<Token, PostError> {
        let Some((id, secret)) = &self.app else {
            return Err(failed("Pinterest is not set up."));
        };
        let req = self
            .client
            .post(self.url("/oauth/token"))
            .basic_auth(id, Some(secret))
            .form(form);
        let (status, body) = self.raw(req, step).await?;
        if !status.is_success() || body["access_token"].is_null() {
            // A refresh token that is revoked, expired or already replaced.
            if step == "Refresh" && matches!(status.as_u16(), 400 | 401) {
                return Err(PostError::Reconnect);
            }
            return Err(refusal(status, &body, step));
        }
        let t: TokenAnswer = serde_json::from_value(body)
            .map_err(|_| failed(format!("{step}: Pinterest sent no token.")))?;
        let Some(access_token) = t.access_token else {
            return Err(failed(format!("{step}: Pinterest sent no token.")));
        };
        Ok(Token {
            access_token,
            expires_in: t
                .expires_in
                .filter(|s| *s > 0)
                .unwrap_or(DEFAULT_EXPIRES_IN),
            refresh_token: t.refresh_token.filter(|r| !r.is_empty()),
            scope: t.scope,
        })
    }

    /// Swaps an authorization code for a token. `continuous_refresh` asks older apps for the
    /// refresh token that renews with every use; newer apps get it anyway.
    pub async fn exchange(&self, code: &str, redirect_uri: &str) -> Result<Token, PostError> {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("continuous_refresh", "true"),
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

    /// The account that consented: its id, `@username`, name and picture.
    pub async fn profile(&self, token: &str) -> Result<Profile, PostError> {
        let req = self
            .client
            .get(self.url("/user_account"))
            .bearer_auth(token);
        let me = self.send(req, "Profile").await?;
        let text = |k: &str| me[k].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        let username = text("username");
        let Some(id) = text("id").or_else(|| username.clone()) else {
            return Err(failed("Profile: Pinterest did not say which account."));
        };
        Ok(Profile {
            id,
            display_name: text("business_name").or_else(|| username.clone()),
            username,
            avatar_url: text("profile_image"),
        })
    }

    /// The account's boards (the ones its token may read), 250 a call, at most four calls.
    /// Secret boards need a scope brain does not ask for, so they are not listed.
    pub async fn boards(&self, token: &str) -> Result<Vec<Board>, PostError> {
        let mut boards = Vec::new();
        let mut bookmark: Option<String> = None;
        for _ in 0..BOARD_PAGES {
            let mut query = vec![("page_size", "250")];
            if let Some(b) = &bookmark {
                query.push(("bookmark", b));
            }
            let req = self
                .client
                .get(self.url("/boards"))
                .query(&query)
                .bearer_auth(token);
            let page = self.send(req, "Boards").await?;
            for item in page["items"].as_array().into_iter().flatten() {
                if let (Some(id), Some(name)) = (item["id"].as_str(), item["name"].as_str()) {
                    boards.push(Board {
                        id: id.to_owned(),
                        name: name.to_owned(),
                        privacy: item["privacy"].as_str().map(str::to_owned),
                    });
                }
            }
            bookmark = page["bookmark"]
                .as_str()
                .filter(|b| !b.is_empty())
                .map(str::to_owned);
            if bookmark.is_none() {
                break;
            }
        }
        Ok(boards)
    }

    /// One board of the account, `None` when it has no such board.
    pub async fn board(&self, token: &str, id: &str) -> Result<Option<Board>, PostError> {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Ok(None);
        }
        let req = self
            .client
            .get(self.url(&format!("/boards/{id}")))
            .bearer_auth(token);
        let (status, body) = self.raw(req, "Board").await?;
        if matches!(status.as_u16(), 403 | 404) {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(refusal(status, &body, "Board"));
        }
        Ok(body["id"].as_str().map(|id| Board {
            id: id.to_owned(),
            name: body["name"].as_str().unwrap_or_default().to_owned(),
            privacy: body["privacy"].as_str().map(str::to_owned),
        }))
    }

    /// Creates the pin and answers its id. Never retried by the caller after an answer: a retry
    /// could pin twice.
    pub async fn post(
        &self,
        token: &str,
        pin: &Pin<'_>,
        media: &Media<'_>,
    ) -> Result<String, PostError> {
        let source = match media {
            Media::Image(url) => json!({"source_type": "image_url", "url": url}),
            Media::Carousel(urls) => json!({
                "source_type": "multiple_image_urls",
                "items": urls.iter().map(|u| json!({"url": u})).collect::<Vec<_>>(),
            }),
            Media::Video(blob, content_type) => {
                let id = self.upload_video(token, blob, content_type).await?;
                json!({"source_type": "video_id", "media_id": id, "cover_image_key_frame_time": 1})
            }
        };
        let mut body = json!({
            "board_id": pin.board_id,
            "media_source": source,
            "description": pin.description,
        });
        for (key, value) in [
            ("title", pin.title),
            ("link", pin.link),
            ("alt_text", pin.alt),
        ] {
            if let Some(v) = value {
                body[key] = json!(v);
            }
        }
        let req = self
            .client
            .post(self.url("/pins"))
            .bearer_auth(token)
            .json(&body);
        let made = self.send(req, "Post").await?;
        match made["id"].as_str().filter(|i| !i.is_empty()) {
            Some(id) => Ok(id.to_owned()),
            None => Err(failed("Post: Pinterest sent no pin id.")),
        }
    }

    /// Registers a video, uploads it to the address Pinterest names and waits until Pinterest has
    /// processed it: the media id the pin is made with. Nothing is public until the pin exists, so
    /// every failure here made no post.
    async fn upload_video(
        &self,
        token: &str,
        blob: &Blob,
        content_type: &str,
    ) -> Result<String, PostError> {
        let req = self
            .client
            .post(self.url("/media"))
            .bearer_auth(token)
            .json(&json!({"media_type": "video"}));
        let reg = self.send(req, "Video").await?;
        let (Some(id), Some(upload)) = (reg["media_id"].as_str(), reg["upload_url"].as_str())
        else {
            return Err(failed("Video: Pinterest named no place to upload it."));
        };
        // Pinterest names a storage address; only a secure one (or this API's own, for tests).
        let same_origin = upload.starts_with(self.api_url.split("/v5").next().unwrap_or_default());
        if !upload.starts_with("https://") && !same_origin {
            return Err(failed("Video: Pinterest named an insecure upload address."));
        }
        let mut form = multipart::Form::new();
        for (key, value) in reg["upload_parameters"].as_object().into_iter().flatten() {
            if let Some(v) = value.as_str() {
                form = form.text(key.clone(), v.to_owned());
            }
        }
        let file = multipart::Part::stream_with_length(blob.body().await?, blob.len())
            .file_name("video")
            .mime_str(content_type)
            .map_err(|_| failed("Video: the file type is not valid."))?;
        let sent = self
            .client
            .post(upload)
            .timeout(PUBLISH_TIMEOUT)
            .multipart(form.part("file", file))
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(error = %e.without_url(), "pinterest video upload failed");
                failed("Video: the upload to Pinterest failed.")
            })?;
        if !sent.status().is_success() {
            tracing::warn!(
                status = sent.status().as_u16(),
                "pinterest refused a video upload"
            );
            return Err(failed("Video: Pinterest refused the upload."));
        }
        self.processed(token, id).await?;
        Ok(id.to_owned())
    }

    /// Asks about the uploaded video until Pinterest has processed it.
    async fn processed(&self, token: &str, id: &str) -> Result<(), PostError> {
        let started = tokio::time::Instant::now();
        loop {
            let req = self
                .client
                .get(self.url(&format!("/media/{id}")))
                .bearer_auth(token);
            // A failed status call (a limit, a 5xx) is asked again until the time is up.
            let got = self.send(req, "Video").await.unwrap_or(Value::Null);
            match got["status"].as_str().unwrap_or_default() {
                "succeeded" => return Ok(()),
                "failed" => return Err(failed("Video: Pinterest could not process the video.")),
                _ => {}
            }
            if started.elapsed() >= POLL_FOR + Duration::from_secs(50) * super::extra_polls() {
                let msg = "Video: Pinterest is still processing the video. Try again later.";
                return Err(failed(super::unsent(msg.into(), true)));
            }
            tokio::time::sleep(self.poll_every).await;
        }
    }

    /// Deletes a pin. One already gone counts as down.
    pub async fn delete_pin(&self, token: &str, id: &str) -> Result<(), PostError> {
        if pin_url(id).is_none() {
            return Err(failed("This is not a Pinterest pin id."));
        }
        let req = self
            .client
            .delete(self.url(&format!("/pins/{id}")))
            .bearer_auth(token);
        let (status, body) = self.raw(req, "Delete").await?;
        if status.is_success() || status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(refusal(status, &body, "Delete"))
        }
    }
}

/// The options' title and link, trimmed, blank as none.
pub fn title_and_link(o: &Options) -> (Option<&str>, Option<&str>) {
    (
        Options::text(o.title.as_ref()),
        Options::text(o.link.as_ref()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authorize_url_asks_for_the_pin_scopes() {
        let p = Pinterest::new(Some(("app".into(), "secret".into()))).unwrap();
        let url = p
            .authorize_url("https://api.example/connect/pinterest/callback", "st")
            .unwrap();
        let parsed = Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(parsed.host_str(), Some("www.pinterest.com"));
        assert_eq!(q["client_id"], "app");
        assert_eq!(q["state"], "st");
        assert_eq!(q["response_type"], "code");
        assert_eq!(
            q["scope"],
            "boards:read,boards:write,pins:read,pins:write,user_accounts:read"
        );
        assert!(!url.contains("secret"));
        assert!(Pinterest::new(None)
            .unwrap()
            .authorize_url("https://x", "st")
            .is_none());
    }

    #[test]
    fn a_token_needs_every_posting_scope() {
        let token = |scope: Option<&str>| Token {
            access_token: "t".into(),
            expires_in: 1,
            refresh_token: None,
            scope: scope.map(str::to_owned),
        };
        // Pinterest answers with spaces; commas are read too.
        assert!(token(Some(
            "boards:read boards:write pins:read pins:write user_accounts:read"
        ))
        .has_all_scopes());
        assert!(
            token(Some("boards:read,pins:read,pins:write,user_accounts:read")).has_all_scopes()
        );
        assert!(!token(Some("boards:read pins:read user_accounts:read")).has_all_scopes());
        assert!(!token(None).has_all_scopes());
    }

    #[test]
    fn refusals_say_what_to_do() {
        let body = |m: &str| json!({"code": 29, "message": m});
        let r = |s: u16, m: &str| refusal(StatusCode::from_u16(s).unwrap(), &body(m), "Post");
        assert!(matches!(
            r(401, "Authentication failed."),
            PostError::Reconnect
        ));
        assert!(matches!(
            r(403, "Missing the pins:write scope"),
            PostError::Denied
        ));
        let PostError::Failed(msg) = r(403, "Not permitted, see https://x.example") else {
            panic!("expected a failure");
        };
        assert_eq!(msg, "Post: Not permitted, see");
        let PostError::Failed(msg) = r(429, "slow down") else {
            panic!("expected a failure");
        };
        assert!(msg.ends_with(super::super::UNSENT), "{msg}");
        let PostError::Failed(msg) = r(400, "") else {
            panic!("expected a failure");
        };
        assert_eq!(msg, "Post: HTTP 400");
    }

    #[test]
    fn links_are_made_only_from_numeric_ids() {
        assert_eq!(
            pin_url("1234567890").as_deref(),
            Some("https://www.pinterest.com/pin/1234567890/")
        );
        assert_eq!(pin_url("12/34"), None);
        assert_eq!(pin_url(""), None);
    }

    #[test]
    fn blank_options_count_as_none() {
        let o = Options {
            board_id: None,
            title: Some("  ".into()),
            link: Some(" https://a.example ".into()),
        };
        assert_eq!(title_and_link(&o), (None, Some("https://a.example")));
    }
}
