//! TikTok (an account): the OAuth code swap and token refresh, the profile lookup, and a direct
//! post of one video or a set of photos that TikTok pulls from brain's media links. A post is
//! `init`, then TikTok downloads the media, so the call that follows polls the publish status
//! until TikTok says it is done. TikTok has no delete call: taking a post down is done in the
//! TikTok app. An access token lasts a day, the refresh token a year. Errors carry no URL, token
//! or secret.
use std::time::Duration;

use reqwest::{RequestBuilder, StatusCode, Url};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use utoipa::ToSchema;

use super::bluesky::PostError;

const AUTH_URL: &str = "https://www.tiktok.com/v2/auth/authorize/";
const API_URL: &str = "https://open.tiktokapis.com";
/// Read the profile, and post directly. TikTok joins scopes with commas.
const SCOPES: &str = "user.info.basic,video.upload,video.publish";
/// Used when TikTok leaves `expires_in` out.
const DEFAULT_EXPIRES_IN: i64 = 86_400;
/// Longest one post (init, TikTok downloading the media, publishing) may take.
pub const PUBLISH_TIMEOUT: Duration = Duration::from_secs(300);
/// How long to wait for TikTok to finish a post before saying it is still working.
const POLL_FOR: Duration = Duration::from_secs(270);
pub const VIDEO_TYPES: [&str; 2] = ["video/mp4", "video/quicktime"];
pub const PHOTO_TYPES: [&str; 2] = ["image/jpeg", "image/webp"];
/// A photo post's title is the first line of the body, cut to what TikTok allows (it counts
/// UTF-16 code units).
const PHOTO_TITLE_MAX: usize = 90;

/// What one post sends: a video, or photos, each by the link TikTok downloads it from.
pub enum Media<'a> {
    Video(&'a str),
    Photos(Vec<&'a str>),
}

/// What the user chose for one post, as TikTok's Content Sharing Guidelines require: who may see
/// it (no default: the user picks one the account is offered), which interactions are allowed
/// (all off unless turned on; duets and stitches only apply to a video), and the commercial
/// content disclosure. A post without settings (made through the API) posts with the most
/// public level the account is offered and every interaction off.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Options {
    /// `PUBLIC_TO_EVERYONE`, `MUTUAL_FOLLOW_FRIENDS`, `FOLLOWER_OF_CREATOR` or `SELF_ONLY`, one
    /// of the account's `privacy_level_options` (`GET /connections/{id}/tiktok`). Required.
    pub privacy_level: Option<String>,
    /// Allow comments.
    #[serde(default)]
    pub allow_comment: bool,
    /// Allow duets (video only).
    #[serde(default)]
    pub allow_duet: bool,
    /// Allow stitches (video only).
    #[serde(default)]
    pub allow_stitch: bool,
    /// The content disclosure switch: the post promotes a brand. Needs `your_brand`,
    /// `branded_content` or both.
    #[serde(default)]
    pub commercial_content: bool,
    /// The creator promotes their own business; TikTok labels the post "Promotional content".
    #[serde(default)]
    pub your_brand: bool,
    /// A paid partnership with a third party; TikTok labels the post "Paid partnership" and it
    /// cannot be `SELF_ONLY`.
    #[serde(default)]
    pub branded_content: bool,
    /// The post is AI-generated.
    #[serde(default)]
    pub ai_generated: bool,
}

/// What TikTok says an account may post, from the creator info call.
#[derive(Debug, Default, PartialEq, Serialize, ToSchema)]
pub struct Creator {
    /// The name to show, so the user knows which account receives the post.
    pub nickname: String,
    pub username: Option<String>,
    /// The privacy levels the account is offered; show only these.
    pub privacy_level_options: Vec<String>,
    /// The creator turned the interaction off in the TikTok app: show it unavailable.
    pub comment_disabled: bool,
    pub duet_disabled: bool,
    pub stitch_disabled: bool,
    /// The longest video the account may post, in seconds.
    pub max_video_post_duration_sec: Option<i64>,
    /// Whether TikTok lets the account post now.
    pub can_post: bool,
    /// Why not, when `can_post` is false.
    pub reason: Option<String>,
}

impl Creator {
    fn blocked(reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
            ..Self::default()
        }
    }

    fn from_data(data: &Value) -> Self {
        let text = |k: &str| {
            data[k]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let options: Vec<String> = data["privacy_level_options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        let username = text("creator_username");
        let can_post = !options.is_empty();
        Self {
            nickname: text("creator_nickname")
                .or_else(|| username.clone())
                .unwrap_or_default(),
            username,
            can_post,
            reason: (!can_post).then(|| CANNOT_POST.to_owned()),
            privacy_level_options: options,
            comment_disabled: data["comment_disabled"].as_bool().unwrap_or(false),
            duet_disabled: data["duet_disabled"].as_bool().unwrap_or(false),
            stitch_disabled: data["stitch_disabled"].as_bool().unwrap_or(false),
            max_video_post_duration_sec: data["max_video_post_duration_sec"].as_i64(),
        }
    }

    /// The most public privacy level offered: `PUBLIC_TO_EVERYONE` when TikTok offers it, else
    /// the first it does.
    fn most_public(&self) -> Option<&str> {
        let o = &self.privacy_level_options;
        o.iter()
            .find(|o| *o == "PUBLIC_TO_EVERYONE")
            .or(o.first())
            .map(String::as_str)
    }
}

const CANNOT_POST: &str = "This TikTok account cannot post right now.";

/// The TikTok app (`TIKTOK_CLIENT_KEY`, `TIKTOK_CLIENT_SECRET`). No `Debug`: it holds the secret.
#[derive(Clone)]
pub struct TikTok {
    /// `(client key, client secret)`; connecting and refreshing need them, calls with a live
    /// access token do not.
    app: Option<(String, String)>,
    /// Tests point these at a mock.
    pub auth_url: String,
    pub api_url: String,
    /// How often the publish status is asked; tests shorten it.
    pub poll_every: Duration,
    client: reqwest::Client,
}

pub struct Token {
    pub access_token: String,
    pub expires_in: i64,
    /// A refresh may bring a new one; the code swap always does.
    pub refresh_token: Option<String>,
    /// The scopes TikTok granted, comma-separated. The user can untick some.
    pub scope: Option<String>,
    pub open_id: Option<String>,
}

impl Token {
    /// Whether every scope this connector asks for was granted. A token that names none counts
    /// as missing them, so the connect fails rather than breaking at the first publish.
    pub fn has_all_scopes(&self) -> bool {
        let granted: Vec<&str> = self
            .scope
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .collect();
        SCOPES.split(',').all(|s| granted.contains(&s))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Profile {
    pub open_id: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    /// The `@username`, without the `@`, when TikTok says it.
    pub username: Option<String>,
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: Option<String>,
    expires_in: Option<i64>,
    refresh_token: Option<String>,
    scope: Option<String>,
    open_id: Option<String>,
}

/// A finished post: the publish id, and TikTok's id of the video when it is public.
pub struct Posted {
    pub publish_id: String,
    pub video_id: Option<String>,
}

fn failed(msg: impl Into<String>) -> PostError {
    PostError::Failed(msg.into())
}

/// TikTok's own words for a refused call, without links, cut short.
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

/// The machine-readable code of a call's answer: `ok` when it worked. The token endpoint puts
/// its error in `error`.
fn code(body: &Value) -> &str {
    body["error"]["code"]
        .as_str()
        .or(body["error"].as_str())
        .unwrap_or_default()
}

/// What a refused call means: a dead token or a missing permission is `Reconnect`, a limit or a
/// review TikTok has not given says so, anything else is `Failed` with `step` and TikTok's reason.
fn refusal(status: StatusCode, body: &Value, step: &str) -> PostError {
    match (status.as_u16(), code(body)) {
        (401, _)
        | (_, "access_token_invalid" | "scope_not_authorized" | "scope_permission_missed") => {
            PostError::Reconnect
        }
        // A revoked or expired refresh token, or a code that was already used.
        (_, "invalid_grant") => PostError::Reconnect,
        (_, "rate_limit_exceeded") => {
            failed("TikTok is limiting this account for now. Try again in a little while.")
        }
        (_, "spam_risk_too_many_posts") => {
            failed("This TikTok account reached its limit of posts for the day.")
        }
        (_, "spam_risk_user_banned_from_posting") => {
            failed("TikTok does not let this account post right now.")
        }
        (_, "unaudited_client_can_only_post_to_private_accounts") => failed(
            "TikTok only lets SocialReins post to private accounts until it passes TikTok's \
             review. Set the account to private in TikTok, then try again.",
        ),
        (_, "url_ownership_unverified") => {
            failed("TikTok could not confirm that SocialReins owns the address of the media.")
        }
        (_, "reached_active_user_cap") => {
            failed("TikTok's limit on accounts using SocialReins is reached for now.")
        }
        _ => failed(format!("{step}: {}", reason(body, status))),
    }
}

/// The first line of `body`, at most `PHOTO_TITLE_MAX` characters.
fn photo_title(body: &str) -> String {
    let mut units = 0;
    body.trim()
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= PHOTO_TITLE_MAX
        })
        .collect()
}

impl TikTok {
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

    /// Whether the client key and secret are set, so accounts can be connected.
    pub fn configured(&self) -> bool {
        self.app.is_some()
    }

    /// Where `GET /connections/tiktok/redirect` sends the browser; `None` when not configured.
    pub fn authorize_url(&self, redirect_uri: &str, state: &str) -> Option<String> {
        let (key, _) = self.app.as_ref()?;
        let params = [
            ("client_key", key.as_str()),
            ("scope", SCOPES),
            ("response_type", "code"),
            ("redirect_uri", redirect_uri),
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
            tracing::warn!(error = %e.without_url(), step, "tiktok call failed");
            failed(format!("{step}: TikTok could not be reached."))
        })?;
        let status = res.status();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        Ok((status, body))
    }

    /// `raw`, with anything not 2xx or not `ok` turned into its `refusal`; the `data` of the answer.
    async fn send(&self, req: RequestBuilder, step: &str) -> Result<Value, PostError> {
        let (status, body) = self.raw(req, step).await?;
        Self::data(status, &body, step)
    }

    /// The `data` of an answer, or its `refusal`.
    fn data(status: StatusCode, body: &Value, step: &str) -> Result<Value, PostError> {
        if status.is_success() && matches!(code(body), "ok" | "") {
            Ok(body["data"].clone())
        } else {
            // TikTok's code and request id help when asking TikTok about a refusal.
            let log_id = body["error"]["log_id"].as_str().unwrap_or_default();
            tracing::warn!(
                step,
                status = status.as_u16(),
                code = code(body),
                log_id,
                "tiktok refused a call"
            );
            Err(refusal(status, body, step))
        }
    }

    async fn token(&self, form: &[(&str, &str)], step: &str) -> Result<Token, PostError> {
        let Some((key, secret)) = &self.app else {
            return Err(failed("TikTok is not set up."));
        };
        let mut form = form.to_vec();
        form.extend([
            ("client_key", key.as_str()),
            ("client_secret", secret.as_str()),
        ]);
        let url = format!("{}/v2/oauth/token/", self.api_url);
        let (status, body) = self.raw(self.client.post(url).form(&form), step).await?;
        if !status.is_success() || body["access_token"].is_null() {
            return Err(refusal(status, &body, step));
        }
        let t: TokenAnswer = serde_json::from_value(body)
            .map_err(|_| failed(format!("{step}: TikTok sent no token.")))?;
        let Some(access_token) = t.access_token else {
            return Err(failed(format!("{step}: TikTok sent no token.")));
        };
        Ok(Token {
            access_token,
            expires_in: t
                .expires_in
                .filter(|s| *s > 0)
                .unwrap_or(DEFAULT_EXPIRES_IN),
            refresh_token: t.refresh_token.filter(|r| !r.is_empty()),
            scope: t.scope,
            open_id: t.open_id.filter(|o| !o.is_empty()),
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

    /// The account that consented: its id, name and picture, and its `@username` when the
    /// account can post (the creator info call; an account that cannot is saved without one).
    pub async fn profile(&self, access_token: &str) -> Result<Profile, PostError> {
        let url = Url::parse_with_params(
            &format!("{}/v2/user/info/", self.api_url),
            [("fields", "open_id,display_name,avatar_url")],
        )
        .map_err(|_| failed("Profile: bad address."))?;
        let data = self
            .send(self.client.get(url).bearer_auth(access_token), "Profile")
            .await?;
        let user = &data["user"];
        let text = |v: &Value| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        let Some(open_id) = text(&user["open_id"]) else {
            return Err(failed("Profile: TikTok did not say which account."));
        };
        let username = self
            .creator(access_token)
            .await
            .ok()
            .and_then(|c| text(&c["creator_username"]));
        Ok(Profile {
            open_id,
            display_name: text(&user["display_name"]),
            avatar_url: text(&user["avatar_url"]),
            username,
        })
    }

    /// What the account may post: its privacy options, among other limits.
    async fn creator(&self, token: &str) -> Result<Value, PostError> {
        let url = format!("{}/v2/post/publish/creator_info/query/", self.api_url);
        self.send(
            self.client.post(url).bearer_auth(token).json(&json!({})),
            "Account",
        )
        .await
    }

    /// What the account may post now. An account TikTok will not let post (a daily limit, a ban,
    /// the app's user cap) is a `Creator` with `can_post` false and the reason, not an error.
    pub async fn creator_info(&self, token: &str) -> Result<Creator, PostError> {
        let url = format!("{}/v2/post/publish/creator_info/query/", self.api_url);
        let req = self.client.post(url).bearer_auth(token).json(&json!({}));
        let (status, body) = self.raw(req, "Account").await?;
        match Self::data(status, &body, "Account") {
            Ok(data) => Ok(Creator::from_data(&data)),
            Err(PostError::Failed(why))
                if matches!(
                    code(&body),
                    "spam_risk_too_many_posts"
                        | "spam_risk_user_banned_from_posting"
                        | "reached_active_user_cap"
                ) =>
            {
                Ok(Creator::blocked(why))
            }
            Err(e) => Err(e),
        }
    }

    /// Posts `media` with `body` as its caption and waits for TikTok to finish. Never retried: a
    /// retry could post twice. `options` are the user's choices; without them (a post made
    /// through the API) the most public level the account is offered is used and comments,
    /// duets and stitches are off.
    pub async fn post(
        &self,
        token: &str,
        body: &str,
        media: &Media<'_>,
        options: Option<&Options>,
    ) -> Result<Posted, PostError> {
        let creator = self.creator_info(token).await?;
        if !creator.can_post {
            return Err(failed(creator.reason.unwrap_or_else(|| CANNOT_POST.into())));
        }
        let privacy = match options {
            Some(o) => {
                let level = o.privacy_level.as_deref().unwrap_or_default();
                if !creator.privacy_level_options.iter().any(|p| p == level) {
                    return Err(failed(
                        "TikTok does not offer the chosen visibility to this account. Choose another and save the post again.",
                    ));
                }
                level
            }
            None => creator.most_public().unwrap_or_default(),
        };
        tracing::info!(options = ?creator.privacy_level_options, chosen = %privacy, "tiktok privacy levels");
        // An interaction is on only when the user turned it on and the creator allows it.
        let o = options.cloned().unwrap_or_default();
        let comment = o.allow_comment && !creator.comment_disabled;
        let duet = o.allow_duet && !creator.duet_disabled;
        let stitch = o.allow_stitch && !creator.stitch_disabled;
        let (path, payload) = match media {
            Media::Video(url) => {
                let mut info = json!({
                    "title": body.trim(),
                    "disable_duet": !duet, "disable_comment": !comment, "disable_stitch": !stitch,
                });
                if options.is_some() {
                    info["brand_content_toggle"] = json!(o.commercial_content && o.branded_content);
                    info["brand_organic_toggle"] = json!(o.commercial_content && o.your_brand);
                    if o.ai_generated {
                        info["is_aigc"] = json!(true);
                    }
                }
                (
                    "video/init",
                    json!({
                        "post_info": info,
                        "source_info": {"source": "PULL_FROM_URL", "video_url": url},
                    }),
                )
            }
            Media::Photos(urls) => {
                let mut info = json!({
                    "title": photo_title(body), "description": body.trim(),
                    "disable_comment": !comment,
                });
                if options.is_some() {
                    info["brand_content_toggle"] = json!(o.commercial_content && o.branded_content);
                    info["brand_organic_toggle"] = json!(o.commercial_content && o.your_brand);
                }
                let mut payload = json!({
                    "post_info": info,
                    "source_info": {
                        "source": "PULL_FROM_URL", "photo_cover_index": 0, "photo_images": urls,
                    },
                    "post_mode": "DIRECT_POST", "media_type": "PHOTO",
                });
                // For a photo post TikTok documents the flag beside `post_info`, not in it.
                if o.ai_generated {
                    payload["is_aigc"] = json!(true);
                }
                ("content/init", payload)
            }
        };
        let url = format!("{}/v2/post/publish/{path}/", self.api_url);
        let init = |level: &str| {
            let mut payload = payload.clone();
            payload["post_info"]["privacy_level"] = json!(level);
            self.raw(
                self.client.post(&url).bearer_auth(token).json(&payload),
                "Post",
            )
        };
        let (mut status, mut answer) = init(privacy).await?;
        // Until TikTok reviews the app it takes only `SELF_ONLY`. A refused init made no post, so
        // asking again with it cannot post twice. Branded content cannot be private, so it is not
        // asked again.
        let offered = creator
            .privacy_level_options
            .iter()
            .any(|p| p == "SELF_ONLY");
        if code(&answer) == "unaudited_client_can_only_post_to_private_accounts"
            && privacy != "SELF_ONLY"
            && offered
            && !(o.commercial_content && o.branded_content)
        {
            (status, answer) = init("SELF_ONLY").await?;
        }
        if !status.is_success() || !matches!(code(&answer), "ok" | "") {
            let log_id = answer["error"]["log_id"].as_str().unwrap_or_default();
            tracing::warn!(
                status = status.as_u16(),
                code = code(&answer),
                log_id,
                "tiktok refused a post"
            );
            return Err(refusal(status, &answer, "Post"));
        }
        let data = answer["data"].clone();
        let Some(publish_id) = data["publish_id"].as_str().filter(|p| !p.is_empty()) else {
            return Err(failed("Post: TikTok sent no post id."));
        };
        self.wait(token, publish_id).await
    }

    /// Asks the publish status until TikTok is done with `publish_id`.
    async fn wait(&self, token: &str, publish_id: &str) -> Result<Posted, PostError> {
        let url = format!("{}/v2/post/publish/status/fetch/", self.api_url);
        let started = tokio::time::Instant::now();
        loop {
            let req = self
                .client
                .post(&url)
                .bearer_auth(token)
                .json(&json!({ "publish_id": publish_id }));
            // The post exists once init answered, so a failed status call (a limit, a 5xx, the
            // network) is asked again until the time is up, never reported as a failed post.
            let data = self.send(req, "Status").await.unwrap_or(Value::Null);
            match data["status"].as_str().unwrap_or_default() {
                "PUBLISH_COMPLETE" => {
                    let video_id = data["publicaly_available_post_id"]
                        .get(0)
                        .map(|v| v.to_string().trim_matches('"').to_owned())
                        .filter(|v| !v.is_empty() && v != "null");
                    return Ok(Posted {
                        publish_id: publish_id.to_owned(),
                        video_id,
                    });
                }
                "FAILED" => {
                    let why = data["fail_reason"].as_str().unwrap_or("unknown");
                    return Err(failed(format!("TikTok could not publish it ({why}).")));
                }
                _ => {}
            }
            if started.elapsed() >= POLL_FOR {
                return Err(failed(
                    "TikTok has not said it is done and may still post it. Check the account before posting again.",
                ));
            }
            tokio::time::sleep(self.poll_every).await;
        }
    }
}

/// The public link to a video, when both the account's `@username` and the video id are known.
pub fn video_url(username: &str, id: &str) -> Option<String> {
    let ok = |s: &str, max: usize| {
        !s.is_empty()
            && s.len() <= max
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.'))
    };
    (ok(username, 64) && ok(id, 32))
        .then(|| format!("https://www.tiktok.com/@{username}/video/{id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authorize_url_asks_for_the_posting_scopes() {
        let t = TikTok::new(Some(("key".into(), "secret".into()))).unwrap();
        let url = t
            .authorize_url("https://api.example/connect/tiktok/callback", "st")
            .unwrap();
        let parsed = Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(parsed.host_str(), Some("www.tiktok.com"));
        assert_eq!(q["client_key"], "key");
        assert_eq!(q["state"], "st");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["scope"], "user.info.basic,video.upload,video.publish");
        assert!(!url.contains("secret"));
        assert!(TikTok::new(None)
            .unwrap()
            .authorize_url("https://x", "st")
            .is_none());
    }

    #[test]
    fn a_token_needs_every_scope() {
        let token = |scope: Option<&str>| Token {
            access_token: "t".into(),
            expires_in: 1,
            refresh_token: None,
            scope: scope.map(str::to_owned),
            open_id: None,
        };
        assert!(token(Some("user.info.basic,video.upload,video.publish")).has_all_scopes());
        assert!(!token(Some("user.info.basic,video.upload")).has_all_scopes());
        assert!(!token(None).has_all_scopes());
    }

    #[test]
    fn the_creator_is_read_from_creator_info() {
        let c = |v: Value| Creator::from_data(&v);
        let full = c(
            json!({"creator_nickname": "Ann", "creator_username": "annlee",
            "privacy_level_options": ["SELF_ONLY", "PUBLIC_TO_EVERYONE"],
            "duet_disabled": true, "max_video_post_duration_sec": 600}),
        );
        assert_eq!(full.nickname, "Ann");
        assert!(full.can_post && full.duet_disabled && !full.comment_disabled);
        assert_eq!(full.max_video_post_duration_sec, Some(600));
        assert_eq!(full.most_public(), Some("PUBLIC_TO_EVERYONE"));
        let private =
            c(json!({"creator_username": "annlee", "privacy_level_options": ["SELF_ONLY"]}));
        assert_eq!(private.most_public(), Some("SELF_ONLY"));
        // No nickname: the username stands in.
        assert_eq!(private.nickname, "annlee");
        for none in [json!({"privacy_level_options": []}), json!({})] {
            let c = c(none);
            assert!(!c.can_post && c.most_public().is_none());
            assert_eq!(c.reason.as_deref(), Some(CANNOT_POST));
        }
    }

    #[test]
    fn refusals_say_what_to_do() {
        let body = |c: &str| json!({"error": {"code": c, "message": "see https://x.example"}});
        let r = |s: u16, c: &str| refusal(StatusCode::from_u16(s).unwrap(), &body(c), "Post");
        assert!(matches!(r(401, "x"), PostError::Reconnect));
        assert!(matches!(
            r(400, "access_token_invalid"),
            PostError::Reconnect
        ));
        assert!(matches!(
            r(403, "scope_not_authorized"),
            PostError::Reconnect
        ));
        let PostError::Failed(msg) = r(403, "unaudited_client_can_only_post_to_private_accounts")
        else {
            panic!("expected a failure");
        };
        assert!(msg.contains("private"));
        let PostError::Failed(msg) = r(400, "invalid_param") else {
            panic!("expected a failure");
        };
        assert_eq!(msg, "Post: see");
    }

    #[test]
    fn links_are_made_only_from_safe_parts() {
        assert_eq!(
            video_url("ann.lee_1", "7312345678901234567").as_deref(),
            Some("https://www.tiktok.com/@ann.lee_1/video/7312345678901234567")
        );
        assert_eq!(video_url("a/b", "1"), None);
        assert_eq!(video_url("ann", "1?x=y"), None);
        assert_eq!(video_url("", "1"), None);
    }

    #[test]
    fn a_photo_title_is_the_first_line_cut_short() {
        assert_eq!(photo_title("  Hello\nworld"), "Hello");
        assert_eq!(photo_title(&"a".repeat(200)).chars().count(), 90);
        assert_eq!(photo_title(""), "");
        // An emoji is two units: 45 of them fill the 90.
        assert_eq!(photo_title(&"🙂".repeat(60)).chars().count(), 45);
    }
}
