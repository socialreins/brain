//! Threads (graph.threads.com): the OAuth code swap for a 60-day token, its refresh, the
//! profile, and publishing. A post is a container (`TEXT`, `IMAGE`, `VIDEO`, or `CAROUSEL` of
//! containers), polled until Threads has fetched its media, then published. Media goes by public
//! URL: Threads downloads it itself. Errors carry no URL, token or secret.
use std::time::Duration;

use reqwest::{Method, Url};
use serde::Deserialize;
use serde_json::Value;

use super::bluesky::PostError;

const AUTH_URL: &str = "https://threads.com/oauth/authorize";
const GRAPH_URL: &str = "https://graph.threads.com";
const VERSION: &str = "v1.0";
const SCOPES: &str = "threads_basic,threads_content_publish,threads_manage_replies,threads_delete";
/// A long-lived token lasts 60 days; used when Threads leaves `expires_in` out.
const DEFAULT_EXPIRES_IN: i64 = 60 * 24 * 3600;
/// Container status checks: now, then after 1, 2, ... `POLLS - 1` times `poll_every`.
const POLLS: u32 = 7;
/// Longest one post (container, polls, the wait and the publish) may take.
pub const PUBLISH_TIMEOUT: Duration = Duration::from_secs(360);
/// Image and video types Threads takes.
pub const MEDIA_TYPES: [&str; 4] = ["image/jpeg", "image/png", "video/mp4", "video/quicktime"];

/// The Threads app (`THREADS_APP_ID`, `THREADS_APP_SECRET`) and the Graph API. No `Debug`: it
/// holds the secret.
#[derive(Clone)]
pub struct Threads {
    /// `(app id, app secret)`; connecting needs them, publishing with a stored token does not.
    app: Option<(String, String)>,
    /// Tests point these at a mock.
    pub auth_url: String,
    pub graph_url: String,
    /// Between a finished container and its publish: a finished text container still failed
    /// when published sooner than about 30 seconds. Tests shorten it, and `poll_every`.
    pub publish_wait: Duration,
    pub poll_every: Duration,
    client: reqwest::Client,
}

/// A long-lived token and its lifetime in seconds.
pub struct Token {
    pub access_token: String,
    pub expires_in: i64,
}

#[derive(Deserialize)]
pub struct Profile {
    pub id: String,
    pub username: Option<String>,
    pub name: Option<String>,
    pub threads_profile_picture_url: Option<String>,
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: String,
    expires_in: Option<i64>,
}

/// One image or video by public URL.
pub struct Item<'a> {
    pub url: &'a str,
    pub video: bool,
}

fn failed(msg: impl Into<String>) -> PostError {
    PostError::Failed(msg.into())
}

/// The Graph API's own words for a refused call (Threads and Meta), without links (they may carry
/// a token), cut short.
pub(super) fn reason(body: &Value, status: reqwest::StatusCode) -> String {
    let msg = body["error"]["message"]
        .as_str()
        .or(body["error_message"].as_str())
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

/// The token is expired, revoked or missing a permission: only a new sign-in helps.
/// Code 190: the token itself is expired or revoked.
pub(super) fn token_dead(body: &Value) -> bool {
    body["error"]["code"].as_i64() == Some(190)
}

/// Codes 10 and 200 to 299: a permission is missing, which says nothing about the token.
fn denied(body: &Value) -> bool {
    body["error"]["code"]
        .as_i64()
        .is_some_and(|c| c == 10 || (200..300).contains(&c))
}

/// What a Graph refusal means for the account: `Reconnect` for a dead token, `Denied` for a
/// missing permission, else nothing.
pub(super) fn dead_or_denied(body: &Value) -> Option<PostError> {
    if token_dead(body) {
        Some(PostError::Reconnect)
    } else {
        denied(body).then_some(PostError::Denied)
    }
}

impl Threads {
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
            graph_url: GRAPH_URL.into(),
            publish_wait: Duration::from_secs(30),
            poll_every: Duration::from_secs(5),
            client,
        })
    }

    /// Whether the app id and secret are set, so accounts can be connected.
    pub fn configured(&self) -> bool {
        self.app.is_some()
    }

    /// Where `GET /connections/threads/redirect` sends the browser; `None` when not configured.
    pub fn authorize_url(&self, redirect_uri: &str, state: &str) -> Option<String> {
        let (id, _) = self.app.as_ref()?;
        let params = [
            ("client_id", id.as_str()),
            ("redirect_uri", redirect_uri),
            ("scope", SCOPES),
            ("response_type", "code"),
            ("state", state),
        ];
        Url::parse_with_params(&self.auth_url, params)
            .ok()
            .map(String::from)
    }

    /// One Graph call: the JSON answer, `Reconnect` when the token is refused, else `Failed`
    /// with `step` and Threads' reason.
    async fn call(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        query: &[(&str, &str)],
        form: Option<&[(&str, &str)]>,
        step: &str,
    ) -> Result<Value, PostError> {
        let mut req = self
            .client
            .request(method, format!("{}/{path}", self.graph_url))
            .query(query);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        if let Some(f) = form {
            req = req.form(f);
        }
        let res = req.send().await.map_err(|e| {
            tracing::warn!(error = %e.without_url(), step, "threads call failed");
            failed(format!("{step}: Threads could not be reached."))
        })?;
        let status = res.status();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(body);
        }
        if let Some(refusal) = dead_or_denied(&body) {
            return Err(refusal);
        }
        Err(failed(format!("{step}: {}", reason(&body, status))))
    }

    /// Swaps an authorization code for a short-lived token, then that for a 60-day one.
    pub async fn exchange(&self, code: &str, redirect_uri: &str) -> Result<Token, PostError> {
        let Some((id, secret)) = &self.app else {
            return Err(failed("Threads is not set up."));
        };
        let form = [
            ("client_id", id.as_str()),
            ("client_secret", secret.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
            ("code", code),
        ];
        let short = self
            .call(
                Method::POST,
                "oauth/access_token",
                None,
                &[],
                Some(&form),
                "Code",
            )
            .await?;
        let short = short["access_token"]
            .as_str()
            .ok_or_else(|| failed("Code: Threads sent no token."))?;
        // Threads takes these only as a GET query; `call` keeps the URL out of every error.
        let query = [
            ("grant_type", "th_exchange_token"),
            ("client_secret", secret.as_str()),
            ("access_token", short),
        ];
        let long = self
            .call(Method::GET, "access_token", None, &query, None, "Token")
            .await?;
        token(long)
    }

    /// Extends a long-lived token (at least a day old, not yet expired) by another 60 days.
    pub async fn refresh(&self, access_token: &str) -> Result<Token, PostError> {
        let query = [
            ("grant_type", "th_refresh_token"),
            ("access_token", access_token),
        ];
        let answer = self
            .call(
                Method::GET,
                "refresh_access_token",
                None,
                &query,
                None,
                "Refresh",
            )
            .await?;
        token(answer)
    }

    pub async fn me(&self, access_token: &str) -> Result<Profile, PostError> {
        let fields = [("fields", "id,username,name,threads_profile_picture_url")];
        let me = self
            .call(
                Method::GET,
                &format!("{VERSION}/me"),
                Some(access_token),
                &fields,
                None,
                "Profile",
            )
            .await?;
        serde_json::from_value(me).map_err(|_| failed("Profile: Threads sent no id."))
    }

    /// Publishes as `user`, as a reply to `reply_to` when given; returns the post's id and,
    /// when Threads tells it, its permalink.
    pub async fn publish_post(
        &self,
        user: &str,
        token: &str,
        text: &str,
        media: &[Item<'_>],
        reply_to: Option<&str>,
    ) -> Result<(String, Option<String>), PostError> {
        let mut top: Vec<(&str, String)> = Vec::new();
        if !text.is_empty() {
            top.push(("text", text.to_owned()));
        }
        if let Some(r) = reply_to {
            top.push(("reply_to_id", r.to_owned()));
        }
        let container = match media {
            [] => {
                top.push(("media_type", "TEXT".into()));
                self.container(user, token, &top).await?
            }
            [one] => {
                top.extend(item(one));
                self.container(user, token, &top).await?
            }
            many => {
                let mut children = Vec::new();
                for m in many {
                    let mut p = item(m);
                    p.push(("is_carousel_item", "true".into()));
                    children.push(self.container(user, token, &p).await?);
                }
                self.finished(token, &children).await?;
                top.push(("media_type", "CAROUSEL".into()));
                top.push(("children", children.join(",")));
                self.container(user, token, &top).await?
            }
        };
        self.finished(token, std::slice::from_ref(&container))
            .await?;
        tokio::time::sleep(self.publish_wait).await;
        let form = [("creation_id", container.as_str())];
        let made = self
            .call(
                Method::POST,
                &format!("{VERSION}/{user}/threads_publish"),
                Some(token),
                &[],
                Some(&form),
                "Publish",
            )
            .await?;
        let id = id_of(&made).ok_or_else(|| failed("Publish: Threads sent no id."))?;
        // Best effort: the post is up, so a failed lookup only leaves the link out.
        let fields = [("fields", "permalink")];
        let link = self
            .call(
                Method::GET,
                &format!("{VERSION}/{id}"),
                Some(token),
                &fields,
                None,
                "Link",
            )
            .await;
        let link = link.ok().and_then(|v| {
            v["permalink"]
                .as_str()
                .filter(|l| l.starts_with("https://"))
                .map(str::to_owned)
        });
        Ok((id, link))
    }

    async fn container(
        &self,
        user: &str,
        token: &str,
        params: &[(&str, String)],
    ) -> Result<String, PostError> {
        let form: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let made = self
            .call(
                Method::POST,
                &format!("{VERSION}/{user}/threads"),
                Some(token),
                &[],
                Some(&form),
                "Create",
            )
            .await?;
        id_of(&made).ok_or_else(|| failed("Create: Threads sent no id."))
    }

    /// Polls containers until all are `FINISHED`, within one shared bound, longer for a large
    /// video (`extra_polls`).
    async fn finished(&self, token: &str, containers: &[String]) -> Result<(), PostError> {
        let mut left: Vec<&String> = containers.iter().collect();
        let polls = POLLS + super::extra_polls();
        for i in 1..=polls {
            let mut still = Vec::new();
            for c in left {
                let fields = [("fields", "status,error_message")];
                let s = self
                    .call(
                        Method::GET,
                        &format!("{VERSION}/{c}"),
                        Some(token),
                        &fields,
                        None,
                        "Status",
                    )
                    .await?;
                match s["status"].as_str() {
                    Some("FINISHED") => {}
                    Some(code @ ("ERROR" | "EXPIRED")) => {
                        let why = s["error_message"].as_str().unwrap_or(code);
                        let why: String = why.chars().take(200).collect();
                        return Err(failed(format!(
                            "Threads could not process the media: {why}"
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
                tokio::time::sleep(self.poll_every * i.min(POLLS)).await;
            }
        }
        Err(failed(
            "Threads did not finish processing the media in time.",
        ))
    }

    /// Needs `threads_delete`; Threads allows 100 deletes a day per profile.
    pub async fn delete_post(&self, token: &str, id: &str) -> Result<(), PostError> {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(failed("This is not a Threads post id."));
        }
        self.call(
            Method::DELETE,
            &format!("{VERSION}/{id}"),
            Some(token),
            &[],
            None,
            "Delete",
        )
        .await
        .map(|_| ())
    }
}

fn item(m: &Item<'_>) -> Vec<(&'static str, String)> {
    if m.video {
        vec![("media_type", "VIDEO".into()), ("video_url", m.url.into())]
    } else {
        vec![("media_type", "IMAGE".into()), ("image_url", m.url.into())]
    }
}

/// Graph ids come as strings; a number is taken too.
pub(super) fn id_of(v: &Value) -> Option<String> {
    match &v["id"] {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn token(v: Value) -> Result<Token, PostError> {
    let t: TokenAnswer =
        serde_json::from_value(v).map_err(|_| failed("Token: Threads sent no token."))?;
    Ok(Token {
        access_token: t.access_token,
        expires_in: t
            .expires_in
            .filter(|s| *s > 0)
            .unwrap_or(DEFAULT_EXPIRES_IN),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_authorize_url_carries_the_scopes_and_state() {
        let t = Threads::new(Some(("app".into(), "secret".into()))).unwrap();
        let url = t
            .authorize_url("https://api.example/connect/threads/callback", "st")
            .unwrap();
        let parsed = Url::parse(&url).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(parsed.host_str(), Some("threads.com"));
        assert_eq!(q["client_id"], "app");
        assert_eq!(
            q["scope"],
            "threads_basic,threads_content_publish,threads_manage_replies,threads_delete"
        );
        assert_eq!(q["state"], "st");
        assert_eq!(q["response_type"], "code");
        assert!(!url.contains("secret"));
        assert!(Threads::new(None)
            .unwrap()
            .authorize_url("https://x", "st")
            .is_none());
    }

    #[test]
    fn refusals_name_the_reason_without_links() {
        let body = json!({"error": {"message": "Bad https://graph.threads.com/x?access_token=abc thing",
            "code": 100}});
        let r = reason(&body, reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(r, "Bad thing");
        assert_eq!(dead_or_denied(&body), None);
        assert_eq!(
            reason(&Value::Null, reqwest::StatusCode::BAD_GATEWAY),
            "HTTP 502"
        );
        // Only 190 says the token is dead; a missing permission must not mark the account.
        let code = |c: i64| dead_or_denied(&json!({"error": {"code": c}}));
        assert_eq!(code(190), Some(PostError::Reconnect));
        for c in [10, 200, 299] {
            assert_eq!(code(c), Some(PostError::Denied), "{c}");
        }
        assert_eq!(code(300), None);
    }

    #[test]
    fn tokens_default_to_60_days() {
        let t = token(json!({"access_token": "a"})).unwrap();
        assert_eq!(t.expires_in, DEFAULT_EXPIRES_IN);
        let t = token(json!({"access_token": "a", "expires_in": 100})).unwrap();
        assert_eq!(t.expires_in, 100);
        assert!(token(json!({})).is_err());
        assert_eq!(id_of(&json!({"id": 17})).as_deref(), Some("17"));
        assert_eq!(id_of(&json!({"id": ""})), None);
    }
}
