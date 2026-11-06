//! Meta (graph.facebook.com): Facebook Login for the Pages a user manages and the Instagram
//! professional accounts linked to them, and publishing to both with the Page's token. Page
//! tokens from a long-lived user token do not expire, so there is no refresh. Media goes by
//! public URL: Meta downloads it itself. Errors carry no URL, token or secret.
use std::time::Duration;

use reqwest::{Method, RequestBuilder, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    bluesky::{InboxError, PostError},
    threads::{dead_or_denied, id_of, reason, Item},
};

const AUTH_URL: &str = "https://www.facebook.com";
const GRAPH_URL: &str = "https://graph.facebook.com";
const RUPLOAD_URL: &str = "https://rupload.facebook.com";
/// `META_GRAPH_VERSION` when unset.
pub const DEFAULT_VERSION: &str = "v26.0";
/// Publishing and take down, then what comment automations will need.
const SCOPES: &str = "pages_show_list,pages_read_engagement,pages_manage_posts,\
business_management,instagram_basic,instagram_content_publish,instagram_manage_contents,\
pages_manage_metadata,pages_messaging,pages_manage_engagement,pages_read_user_content,\
instagram_manage_comments,instagram_manage_messages";
/// What a Page subscribes to. Instagram messages and comments come through the same Page
/// subscription (the app's Instagram webhooks need no field of their own here).
const WEBHOOK_FIELDS: &str = "feed,messages";
const PAGE_FIELDS: &str =
    "id,name,access_token,picture,instagram_business_account{id,username,profile_picture_url}";
/// Pages read per call, and calls at most: 1000 Pages.
const PAGE_LIMIT: &str = "100";
const PAGE_CALLS: usize = 10;
/// Status checks: now, then after 1, 2, ... `POLLS - 1` times `poll_every` (about 4 minutes).
const POLLS: u32 = 10;
/// A carousel container's own wait, after its items finished (about 50 seconds).
const CAROUSEL_POLLS: u32 = 5;
/// Longest one post may take: an Instagram video is processed for minutes.
pub const PUBLISH_TIMEOUT: Duration = Duration::from_secs(420);
/// Longest the permalink lookup after an Instagram publish may take.
const LINK_TIMEOUT: Duration = Duration::from_secs(5);
/// What each platform takes. Instagram publishes JPEG images only.
pub const FACEBOOK_TYPES: [&str; 5] = [
    "image/jpeg",
    "image/png",
    "image/gif",
    "video/mp4",
    "video/quicktime",
];
pub const INSTAGRAM_TYPES: [&str; 3] = ["image/jpeg", "video/mp4", "video/quicktime"];

/// The Meta app (`META_APP_ID`, `META_APP_SECRET`) and the Graph API. No `Debug`: it holds the
/// secret.
#[derive(Clone)]
pub struct Meta {
    /// `(app id, app secret)`; connecting needs them, publishing with a stored token does not.
    app: Option<(String, String)>,
    /// `META_GRAPH_VERSION`, e.g. `v26.0`.
    version: String,
    /// Tests point these at a mock, and shorten `poll_every`.
    pub auth_url: String,
    pub graph_url: String,
    pub rupload_url: String,
    pub poll_every: Duration,
    /// `META_WEBHOOK_VERIFY_TOKEN`: the subscribe handshake of `GET /webhooks/meta`; the
    /// webhook is off without it.
    pub webhook_token: Option<String>,
    client: reqwest::Client,
}

/// The Facebook and Instagram publishers, both on the one Meta app.
#[derive(Clone)]
pub struct Facebook(pub Meta);
#[derive(Clone)]
pub struct Instagram(pub Meta);

/// A Page the user granted, with its token and linked Instagram account. Held only sealed, while
/// the user picks (`controllers/meta.rs`).
#[derive(Serialize, Deserialize, Clone)]
pub struct Page {
    pub id: String,
    pub name: String,
    pub access_token: String,
    pub picture_url: Option<String>,
    pub instagram: Option<IgAccount>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct IgAccount {
    pub id: String,
    pub username: Option<String>,
    pub picture_url: Option<String>,
}

/// A query or form: name and value pairs.
type Params<'a> = &'a [(&'a str, &'a str)];

/// A call that did not succeed: not reached, or Graph's status and error body.
enum Refused {
    Unreachable,
    Graph(reqwest::StatusCode, Value),
}

/// A reply or message that Meta did not take, as the inbox words it.
fn inbox_error(refused: Refused) -> InboxError {
    let Refused::Graph(status, body) = refused else {
        return InboxError::Meta("Meta could not be reached.".into());
    };
    let code = body["error"]["code"].as_i64();
    let sub = body["error"]["error_subcode"].as_i64();
    match (code, sub) {
        // Outside the 24 hour messaging window.
        (_, Some(2_018_278 | 2_534_022)) => InboxError::Invalid("outside_window"),
        (Some(4 | 17 | 32 | 613), _) => InboxError::RateLimited,
        // "No matching user", "This person isn't available right now".
        (Some(551), _) | (_, Some(2_018_001)) => InboxError::Invalid("dm_unavailable"),
        // Only a bad token asks for a reconnect here: codes 10 and 200 to 299 are also how Meta
        // words messaging-policy refusals (a second private reply, a blocked recipient), which
        // a new login would not fix.
        (Some(190), _) => InboxError::MetaReconnect,
        _ => InboxError::Meta(reason(&body, status)),
    }
}

/// A person's name, handle and picture as Meta shows them.
#[derive(Debug, PartialEq, Eq)]
pub struct Profile {
    pub name: Option<String>,
    pub handle: Option<String>,
    pub avatar: Option<String>,
}

/// Who a message goes to.
#[derive(Clone, Copy)]
pub enum To<'a> {
    /// The author of this comment: a private reply.
    Comment(&'a str),
    /// This person, who wrote to the Page within 24 hours.
    Person(&'a str),
}

fn failed(msg: impl Into<String>) -> PostError {
    PostError::Failed(msg.into())
}

/// Graph ids are digits, a Page post's `<page>_<post>`; anything else never reaches a path.
fn graph_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit() || b == b'_')
}

fn text(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// One Page of `me/accounts`; `None` without an id, name or token.
fn page(v: &Value) -> Option<Page> {
    let id = id_of(v).filter(|i| graph_id(i))?;
    let ig = &v["instagram_business_account"];
    let instagram = id_of(ig).filter(|i| graph_id(i)).map(|id| IgAccount {
        id,
        username: text(&ig["username"]),
        picture_url: text(&ig["profile_picture_url"]),
    });
    Some(Page {
        id,
        name: text(&v["name"])?,
        access_token: text(&v["access_token"])?,
        picture_url: text(&v["picture"]["data"]["url"]),
        instagram,
    })
}

impl Meta {
    pub fn new(app: Option<(String, String)>, version: &str) -> Result<Self, String> {
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let valid = version
            .strip_prefix('v')
            .and_then(|v| v.split_once('.'))
            .is_some_and(|(major, minor)| digits(major) && digits(minor));
        if !valid {
            return Err("META_GRAPH_VERSION: expected a version like v26.0".into());
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("SocialReins")
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        Ok(Self {
            app,
            version: version.into(),
            auth_url: AUTH_URL.into(),
            graph_url: GRAPH_URL.into(),
            rupload_url: RUPLOAD_URL.into(),
            poll_every: Duration::from_secs(5),
            webhook_token: None,
            client,
        })
    }

    /// Whether `header` (`X-Hub-Signature-256`) signs `body` with the app secret. Never true
    /// without one.
    pub fn signed(&self, body: &[u8], header: &str) -> bool {
        self.app.as_ref().is_some_and(|(_, secret)| {
            socialreins_core::webhook::verify_sha256(secret, body, header)
        })
    }

    /// Whether the app id and secret are set, so accounts can be connected.
    pub fn configured(&self) -> bool {
        self.app.is_some()
    }

    /// Where `GET /connections/meta/redirect` sends the browser; `None` when not configured.
    pub fn authorize_url(&self, redirect_uri: &str, state: &str) -> Option<String> {
        let (id, _) = self.app.as_ref()?;
        let params = [
            ("client_id", id.as_str()),
            ("redirect_uri", redirect_uri),
            ("state", state),
            ("scope", SCOPES),
            ("response_type", "code"),
        ];
        let dialog = format!("{}/{}/dialog/oauth", self.auth_url, self.version);
        Url::parse_with_params(&dialog, params)
            .ok()
            .map(String::from)
    }

    /// Sends one request: the JSON answer, or why not. Never logs the URL or token.
    async fn raw(
        &self,
        mut req: RequestBuilder,
        token: Option<&str>,
        form: Option<&[(&str, &str)]>,
        step: &str,
    ) -> Result<Value, Refused> {
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        if let Some(f) = form {
            req = req.form(f);
        }
        let res = req.send().await.map_err(|e| {
            tracing::warn!(error = %e.without_url(), step, "meta call failed");
            Refused::Unreachable
        })?;
        let status = res.status();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(body);
        }
        Err(Refused::Graph(status, body))
    }

    /// Sends one request: the JSON answer, `Reconnect` when the token is refused, else `Failed`
    /// with `step` and Meta's reason.
    async fn send(
        &self,
        req: RequestBuilder,
        token: Option<&str>,
        form: Option<&[(&str, &str)]>,
        step: &str,
    ) -> Result<Value, PostError> {
        self.raw(req, token, form, step)
            .await
            .map_err(|refused| match refused {
                Refused::Unreachable => failed(format!("{step}: Meta could not be reached.")),
                Refused::Graph(_, body) if dead_or_denied(&body).is_some() => {
                    dead_or_denied(&body).unwrap_or(PostError::Reconnect)
                }
                Refused::Graph(status, body) => {
                    failed(format!("{step}: {}", reason(&body, status)))
                }
            })
    }

    /// One Graph API call on `path` under the version.
    async fn call(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        (query, form): (Params<'_>, Option<Params<'_>>),
        step: &str,
    ) -> Result<Value, PostError> {
        let url = format!("{}/{}/{path}", self.graph_url, self.version);
        let req = self.client.request(method, url).query(query);
        self.send(req, token, form, step).await
    }

    async fn post(
        &self,
        path: &str,
        token: &str,
        form: &[(&str, &str)],
        step: &str,
    ) -> Result<Value, PostError> {
        self.call(Method::POST, path, Some(token), (&[], Some(form)), step)
            .await
    }

    async fn get(
        &self,
        path: &str,
        token: &str,
        query: &[(&str, &str)],
        step: &str,
    ) -> Result<Value, PostError> {
        self.call(Method::GET, path, Some(token), (query, None), step)
            .await
    }

    /// Swaps an authorization code for a short-lived user token, then that for a long-lived one.
    /// The secret goes in form bodies only.
    pub async fn exchange(&self, code: &str, redirect_uri: &str) -> Result<String, PostError> {
        let Some((id, secret)) = &self.app else {
            return Err(failed("Meta is not set up."));
        };
        let form = [
            ("client_id", id.as_str()),
            ("client_secret", secret.as_str()),
            ("redirect_uri", redirect_uri),
            ("code", code),
        ];
        let path = "oauth/access_token";
        let short = self
            .call(Method::POST, path, None, (&[], Some(&form)), "Code")
            .await?;
        let short = text(&short["access_token"]).ok_or_else(|| failed("Code: no token."))?;
        let form = [
            ("client_id", id.as_str()),
            ("client_secret", secret.as_str()),
            ("grant_type", "fb_exchange_token"),
            ("fb_exchange_token", short.as_str()),
        ];
        let long = self
            .call(Method::POST, path, None, (&[], Some(&form)), "Token")
            .await?;
        text(&long["access_token"]).ok_or_else(|| failed("Token: no token."))
    }

    /// The Pages the user granted, with their tokens and linked Instagram accounts, following
    /// the paging cursor up to 1000 Pages.
    pub async fn pages(&self, user_token: &str) -> Result<Vec<Page>, PostError> {
        let mut pages = Vec::new();
        let mut after: Option<String> = None;
        for _ in 0..PAGE_CALLS {
            let mut query = vec![("fields", PAGE_FIELDS), ("limit", PAGE_LIMIT)];
            if let Some(a) = &after {
                query.push(("after", a.as_str()));
            }
            let got = self.get("me/accounts", user_token, &query, "Pages").await?;
            let data = got["data"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            pages.extend(data.iter().filter_map(page));
            after = match (
                &got["paging"]["next"],
                text(&got["paging"]["cursors"]["after"]),
            ) {
                (Value::String(_), Some(a)) if !data.is_empty() => Some(a),
                _ => return Ok(pages),
            };
        }
        // shortcut: stops at 1000 Pages; raise PAGE_CALLS if a user ever manages more.
        Ok(pages)
    }

    /// Publishes on Page `page`: text to the feed, one image as a photo, several as unpublished
    /// photos attached to one feed post, one video as a Reel. Returns the post's id.
    pub async fn publish_facebook(
        &self,
        page: &str,
        token: &str,
        message: &str,
        media: &[Item<'_>],
    ) -> Result<String, PostError> {
        if !graph_id(page) {
            return Err(failed("This is not a Facebook Page id."));
        }
        let mut form: Vec<(&str, &str)> = Vec::new();
        let made =
            match media {
                [] => {
                    form.push(("message", message));
                    self.post(&format!("{page}/feed"), token, &form, "Post")
                        .await?
                }
                [one] if one.video => return self.reel(page, token, one.url, message).await,
                [one] => {
                    form.push(("url", one.url));
                    if !message.is_empty() {
                        form.push(("caption", message));
                    }
                    let made = self
                        .post(&format!("{page}/photos"), token, &form, "Photo")
                        .await?;
                    // A photo answers its photo id and the id of the post that shows it.
                    match text(&made["post_id"]) {
                        Some(post) => return Ok(post),
                        None => made,
                    }
                }
                many if many.iter().any(|m| m.video) => return Err(failed(
                    "Facebook cannot mix a video with other media: post the video on its own as \
                     a Reel.",
                )),
                many => {
                    let mut attached = Vec::new();
                    for m in many {
                        let photo = [("url", m.url), ("published", "false")];
                        let made = self
                            .post(&format!("{page}/photos"), token, &photo, "Photo")
                            .await?;
                        let id = id_of(&made).ok_or_else(|| failed("Photo: Meta sent no id."))?;
                        attached.push(serde_json::json!({ "media_fbid": id }).to_string());
                    }
                    let keys: Vec<String> = (0..attached.len())
                        .map(|i| format!("attached_media[{i}]"))
                        .collect();
                    let mut feed: Vec<(&str, &str)> = keys
                        .iter()
                        .map(String::as_str)
                        .zip(attached.iter().map(String::as_str))
                        .collect();
                    if !message.is_empty() {
                        feed.push(("message", message));
                    }
                    self.post(&format!("{page}/feed"), token, &feed, "Post")
                        .await?
                }
            };
        id_of(&made).ok_or_else(|| failed("Post: Meta sent no id."))
    }

    /// A Reel: start an upload session, have Meta fetch the file, wait for it, then publish.
    /// Nothing is public before the finish call. Returns the video id.
    async fn reel(
        &self,
        page: &str,
        token: &str,
        url: &str,
        description: &str,
    ) -> Result<String, PostError> {
        let path = format!("{page}/video_reels");
        let started = self
            .post(&path, token, &[("upload_phase", "start")], "Reel")
            .await?;
        let video = id_of(&serde_json::json!({ "id": started["video_id"].clone() }))
            .filter(|v| graph_id(v))
            .ok_or_else(|| failed("Reel: Meta sent no video id."))?;
        // rupload takes the token as `OAuth <token>` and the file's URL in a header.
        let upload = self
            .client
            .post(format!(
                "{}/video-upload/{}/{video}",
                self.rupload_url, self.version
            ))
            .header("authorization", format!("OAuth {token}"))
            .header("file_url", url);
        self.send(upload, None, None, "Upload").await?;
        for i in 1..=POLLS {
            let got = self
                .get(&video, token, &[("fields", "status")], "Status")
                .await?;
            let status = &got["status"];
            if let Some(code @ ("error" | "expired" | "upload_failed")) =
                status["video_status"].as_str()
            {
                return Err(failed(format!(
                    "Facebook could not take the video ({code})."
                )));
            }
            // Meta's docs spell it both ways.
            if matches!(
                status["uploading_phase"]["status"].as_str(),
                Some("complete" | "completed")
            ) {
                let mut form = vec![
                    ("upload_phase", "finish"),
                    ("video_id", video.as_str()),
                    ("video_state", "PUBLISHED"),
                ];
                if !description.is_empty() {
                    form.push(("description", description));
                }
                self.post(&path, token, &form, "Publish").await?;
                return Ok(video);
            }
            if i < POLLS {
                tokio::time::sleep(self.poll_every * i).await;
            }
        }
        Err(failed(
            "Facebook did not finish receiving the video in time.",
        ))
    }

    /// Sets the cover of video `video` (a Reel) as its preferred thumbnail:
    /// `POST /{video-id}/thumbnails` with the image as `source` and `is_preferred=true`, the
    /// call Meta's Reels guide points to for "a custom cover photo". The upload is a multipart
    /// body, written by hand to keep reqwest's multipart feature out. Once, never retried.
    pub async fn set_thumbnail(
        &self,
        token: &str,
        video: &str,
        (bytes, content_type): (&[u8], &str),
    ) -> Result<(), PostError> {
        if !graph_id(video) {
            return Err(failed("This is not a Facebook video id."));
        }
        let edge = "srcover7d3f1b";
        let ext = if content_type == "image/png" {
            "png"
        } else {
            "jpg"
        };
        let mut body = format!(
            "--{edge}\r\nContent-Disposition: form-data; name=\"is_preferred\"\r\n\r\ntrue\r\n\
             --{edge}\r\nContent-Disposition: form-data; name=\"source\"; \
             filename=\"cover.{ext}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{edge}--\r\n").as_bytes());
        let url = format!("{}/{}/{video}/thumbnails", self.graph_url, self.version);
        let req = self
            .client
            .post(url)
            .header(
                "content-type",
                format!("multipart/form-data; boundary={edge}"),
            )
            .body(body);
        let done = self.send(req, Some(token), None, "Cover").await?;
        if done["success"] == false {
            return Err(failed("Cover: Meta did not accept the image."));
        }
        Ok(())
    }

    /// Publishes on Instagram account `ig`: a container per item (a Reel for one video, with its
    /// `cover` image's URL when given, a carousel for several), polled until `FINISHED`, then published. Returns the media id and,
    /// when Meta tells it, its permalink.
    pub async fn publish_instagram(
        &self,
        ig: &str,
        token: &str,
        caption: &str,
        media: &[Item<'_>],
        cover: Option<&str>,
    ) -> Result<(String, Option<String>), PostError> {
        if !graph_id(ig) {
            return Err(failed("This is not an Instagram account id."));
        }
        let path = format!("{ig}/media");
        let mut top: Vec<(&str, &str)> = Vec::new();
        if !caption.is_empty() {
            top.push(("caption", caption));
        }
        // A carousel waits for its items first, so its own wait is shorter: both stay well
        // inside PUBLISH_TIMEOUT, never cutting off `media_publish`.
        let (container, polls) = match media {
            [] => return Err(failed("Instagram needs at least one image or video.")),
            [one] => {
                if one.video {
                    top.extend([("media_type", "REELS"), ("video_url", one.url)]);
                    // `cover_url`: a JPEG on a public server, used over any frame offset.
                    if let Some(cover) = cover {
                        top.push(("cover_url", cover));
                    }
                } else {
                    top.push(("image_url", one.url));
                }
                (self.container(&path, token, &top).await?, POLLS)
            }
            many => {
                let mut children = Vec::new();
                for m in many {
                    let item = if m.video {
                        [("media_type", "VIDEO"), ("video_url", m.url)]
                    } else {
                        [("media_type", "IMAGE"), ("image_url", m.url)]
                    };
                    let mut form = item.to_vec();
                    form.push(("is_carousel_item", "true"));
                    children.push(self.container(&path, token, &form).await?);
                }
                self.finished(token, &children, POLLS).await?;
                let children = children.join(",");
                top.extend([("media_type", "CAROUSEL"), ("children", children.as_str())]);
                (self.container(&path, token, &top).await?, CAROUSEL_POLLS)
            }
        };
        self.finished(token, std::slice::from_ref(&container), polls)
            .await?;
        let form = [("creation_id", container.as_str())];
        let made = self
            .post(&format!("{ig}/media_publish"), token, &form, "Publish")
            .await?;
        let id = id_of(&made).ok_or_else(|| failed("Publish: Meta sent no id."))?;
        // Best effort: the post is up, so a failed lookup only leaves the link out. Bounded, so
        // a slow lookup cannot run past the publish timeout and lose the live post's id.
        let lookup = self.get(&id, token, &[("fields", "permalink")], "Link");
        let link = tokio::time::timeout(LINK_TIMEOUT, lookup)
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(|v| text(&v["permalink"]))
            .filter(|l| l.starts_with("https://"));
        Ok((id, link))
    }

    async fn container(
        &self,
        path: &str,
        token: &str,
        form: &[(&str, &str)],
    ) -> Result<String, PostError> {
        let made = self.post(path, token, form, "Create").await?;
        id_of(&made).ok_or_else(|| failed("Create: Meta sent no id."))
    }

    /// Polls Instagram containers until all are `FINISHED`, within one shared bound.
    async fn finished(
        &self,
        token: &str,
        containers: &[String],
        polls: u32,
    ) -> Result<(), PostError> {
        let mut left: Vec<&String> = containers.iter().collect();
        for i in 1..=polls {
            let mut still = Vec::new();
            for c in left {
                let fields = [("fields", "status_code,status")];
                let s = self.get(c, token, &fields, "Status").await?;
                match s["status_code"].as_str() {
                    Some("FINISHED" | "PUBLISHED") => {}
                    Some(code @ ("ERROR" | "EXPIRED")) => {
                        let why = s["status"].as_str().unwrap_or(code);
                        let why: String = why.chars().take(200).collect();
                        return Err(failed(format!(
                            "Instagram could not process the media: {why}"
                        )));
                    }
                    _ => still.push(c),
                }
            }
            left = still;
            if left.is_empty() {
                return Ok(());
            }
            if i < polls {
                tokio::time::sleep(self.poll_every * i).await;
            }
        }
        Err(failed(
            "Instagram did not finish processing the media in time.",
        ))
    }

    /// Subscribes Page `page` to the app's webhooks (`feed` and `messages`), so its comments and
    /// messages, and its linked Instagram account's, reach `POST /webhooks/meta`. Needs
    /// `pages_manage_metadata`.
    pub async fn subscribe_page(&self, page: &str, token: &str) -> Result<(), PostError> {
        if !graph_id(page) {
            return Err(failed("This is not a Facebook Page id."));
        }
        let form = [("subscribed_fields", WEBHOOK_FIELDS)];
        let path = format!("{page}/subscribed_apps");
        let done = self.post(&path, token, &form, "Webhook").await?;
        match done["success"] {
            Value::Bool(false) => Err(failed("Webhook: Meta did not subscribe the Page.")),
            _ => Ok(()),
        }
    }

    /// Takes the app's webhook subscription off Page `page`.
    pub async fn unsubscribe_page(&self, page: &str, token: &str) -> Result<(), PostError> {
        if !graph_id(page) {
            return Err(failed("This is not a Facebook Page id."));
        }
        let path = format!("{page}/subscribed_apps");
        self.call(Method::DELETE, &path, Some(token), (&[], None), "Webhook")
            .await
            .map(|_| ())
    }

    /// One call of the inbox: Meta's answer, or why it did not take it. Never retried.
    async fn inbox_call(
        &self,
        method: Method,
        path: &str,
        token: &str,
        form: Option<Params<'_>>,
    ) -> Result<Value, InboxError> {
        let url = format!("{}/{}/{path}", self.graph_url, self.version);
        let req = self.client.request(method, url);
        self.raw(req, Some(token), form, "Inbox")
            .await
            .map_err(inbox_error)
    }

    /// A public reply under `comment`: `{comment}/comments` on Facebook (`pages_manage_engagement`),
    /// `{comment}/replies` on Instagram (`instagram_manage_comments`). Returns the reply's id.
    pub async fn reply_to_comment(
        &self,
        instagram: bool,
        token: &str,
        comment: &str,
        message: &str,
    ) -> Result<String, InboxError> {
        if !graph_id(comment) {
            return Err(InboxError::Invalid("not_repliable"));
        }
        let edge = if instagram { "replies" } else { "comments" };
        let form = [("message", message)];
        let path = format!("{comment}/{edge}");
        let made = self
            .inbox_call(Method::POST, &path, token, Some(&form))
            .await?;
        id_of(&made).ok_or_else(|| InboxError::Meta("Meta sent no id.".into()))
    }

    /// A message from Page `page` (`pages_messaging`, `instagram_manage_messages`): the private
    /// reply to a comment (once per comment, within 7 days), or a DM to a person who wrote
    /// within 24 hours. Returns the message id.
    pub async fn message(
        &self,
        page: &str,
        token: &str,
        to: To<'_>,
        body: &str,
    ) -> Result<String, InboxError> {
        let (To::Comment(id) | To::Person(id)) = to;
        if !graph_id(page) || !graph_id(id) {
            return Err(InboxError::Invalid("not_repliable"));
        }
        let recipient = match to {
            To::Comment(id) => serde_json::json!({ "comment_id": id }),
            To::Person(id) => serde_json::json!({ "id": id }),
        }
        .to_string();
        let message = serde_json::json!({ "text": body }).to_string();
        let mut form = vec![
            ("recipient", recipient.as_str()),
            ("message", message.as_str()),
        ];
        if matches!(to, To::Person(_)) {
            form.push(("messaging_type", "RESPONSE"));
        }
        let path = format!("{page}/messages");
        let made = self
            .inbox_call(Method::POST, &path, token, Some(&form))
            .await?;
        text(&made["message_id"]).ok_or_else(|| InboxError::Meta("Meta sent no id.".into()))
    }

    /// The name, handle and picture of a person who messaged: a Messenger page-scoped id, or an
    /// Instagram-scoped id. `None` when Meta will not say.
    pub async fn profile(&self, instagram: bool, token: &str, id: &str) -> Option<Profile> {
        if !graph_id(id) {
            return None;
        }
        let fields = if instagram {
            "name,username,profile_pic"
        } else {
            "name,profile_pic"
        };
        let got = self
            .get(id, token, &[("fields", fields)], "Profile")
            .await
            .ok()?;
        Some(Profile {
            name: text(&got["name"]),
            handle: text(&got["username"]),
            avatar: text(&got["profile_pic"]),
        })
    }

    /// The public link of an Instagram media.
    pub async fn permalink(&self, token: &str, media: &str) -> Option<String> {
        if !graph_id(media) {
            return None;
        }
        let got = self
            .get(media, token, &[("fields", "permalink")], "Permalink")
            .await
            .ok()?;
        text(&got["permalink"])
    }

    /// Comments `message` on a Facebook post, photo or Reel (`pages_manage_engagement`) or an
    /// Instagram media (`instagram_manage_comments`): `POST {id}/comments` with the Page token,
    /// so it comes from the Page or the Instagram account. Returns the comment's id. Once, never
    /// retried.
    pub async fn comment(&self, token: &str, id: &str, message: &str) -> Result<String, PostError> {
        if !graph_id(id) {
            return Err(failed("This is not a Meta post id."));
        }
        let made = self
            .post(
                &format!("{id}/comments"),
                token,
                &[("message", message)],
                "Comment",
            )
            .await?;
        id_of(&made).ok_or_else(|| failed("Comment: Meta sent no id."))
    }

    /// Deletes a Facebook post, photo or Reel, or an Instagram media, with the Page token.
    pub async fn delete(&self, token: &str, id: &str) -> Result<(), PostError> {
        if !graph_id(id) {
            return Err(failed("This is not a Meta post id."));
        }
        self.call(Method::DELETE, id, Some(token), (&[], None), "Delete")
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn refused_messages_are_worded_for_the_inbox() {
        let refused = |code: i64, sub: Option<i64>| {
            let body = json!({"error": {"message": "nope", "code": code, "error_subcode": sub}});
            inbox_error(Refused::Graph(reqwest::StatusCode::BAD_REQUEST, body))
        };
        assert_eq!(
            refused(10, Some(2_018_278)),
            InboxError::Invalid("outside_window")
        );
        assert_eq!(
            refused(10, Some(2_534_022)),
            InboxError::Invalid("outside_window")
        );
        assert_eq!(refused(613, None), InboxError::RateLimited);
        assert_eq!(refused(551, None), InboxError::Invalid("dm_unavailable"));
        assert_eq!(
            refused(100, Some(2_018_001)),
            InboxError::Invalid("dm_unavailable")
        );
        assert_eq!(refused(190, None), InboxError::MetaReconnect);
        for policy in [10, 200, 230, 299] {
            assert_eq!(
                refused(policy, None),
                InboxError::Meta("nope".into()),
                "{policy}"
            );
        }
        assert_eq!(refused(100, None), InboxError::Meta("nope".into()));
        assert_eq!(
            inbox_error(Refused::Unreachable),
            InboxError::Meta("Meta could not be reached.".into())
        );
    }

    #[test]
    fn the_dialog_url_carries_the_version_scopes_and_state() {
        let m = Meta::new(Some(("app".into(), "secret".into())), "v26.0").unwrap();
        let url = m
            .authorize_url("https://api.example/connect/meta/callback", "st")
            .unwrap();
        let parsed = Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(parsed.host_str(), Some("www.facebook.com"));
        assert_eq!(parsed.path(), "/v26.0/dialog/oauth");
        assert_eq!(q["client_id"], "app");
        assert_eq!(q["state"], "st");
        assert!(q["scope"].starts_with("pages_show_list,pages_read_engagement,"));
        assert!(q["scope"].contains("instagram_manage_contents"));
        assert!(!url.contains("secret"));
        assert!(Meta::new(None, "v26.0")
            .unwrap()
            .authorize_url("https://x", "st")
            .is_none());
        for bad in ["26.0", "v26", "v26.x", "v/26.0", ""] {
            assert!(Meta::new(None, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn pages_keep_their_instagram_account_and_need_a_token() {
        let p = page(&json!({"id": "101", "name": "Cafe", "access_token": "pt",
            "picture": {"data": {"url": "https://scontent.example/p.jpg"}},
            "instagram_business_account": {"id": "1784", "username": "cafe"}}))
        .unwrap();
        assert_eq!(p.id, "101");
        assert_eq!(
            p.picture_url.as_deref(),
            Some("https://scontent.example/p.jpg")
        );
        let ig = p.instagram.unwrap();
        assert_eq!(
            (ig.id.as_str(), ig.username.as_deref()),
            ("1784", Some("cafe"))
        );
        assert!(page(&json!({"id": "101", "name": "Cafe"})).is_none());
        assert!(page(&json!({"id": "../x", "name": "C", "access_token": "t"})).is_none());
        let plain = page(&json!({"id": 7, "name": "C", "access_token": "t"})).unwrap();
        assert!(plain.instagram.is_none());
        assert!(graph_id("101_202") && !graph_id("") && !graph_id("1/2"));
    }
}
