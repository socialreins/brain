//! X inbox calls: mentions and replies, direct messages, a reply, a DM, like, repost and hide a
//! reply. X bills a read per resource returned (a post $0.005, a user or a DM event $0.01) and a
//! write per request, so callers count what came back (`cost_micros`) and never retry a refusal.
//! Nothing here holds a token past a call or puts one in an error.
use chrono::{DateTime, FixedOffset, Utc};
use reqwest::{header::HeaderMap, Method, StatusCode};
use serde_json::{json, Value};

use super::{out_of_credits, plain_id, reason, X};
use crate::inbox::Attachment;

/// X's pay-per-use prices for what an inbox read returns, in millionths of a US dollar (read
/// 2026-11-25 from docs.x.com/x-api/getting-started/pricing; X may change them). A user that an
/// expansion returns is counted as billed, though whether X bills it is unconfirmed: spending
/// is capped by what is counted, so counting too much only stops a little early.
pub const POST_READ_MICROS: i64 = 5_000;
pub const USER_READ_MICROS: i64 = 10_000;
pub const DM_READ_MICROS: i64 = 10_000;

/// What X refused, in terms a poller can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// The token is dead or revoked: connect the account again.
    Reconnect,
    /// The app has no API credits: stop until someone adds credits and resumes.
    Credits,
    /// The app's usage cap is reached: stop until the cap is raised and a person resumes.
    UsageCap,
    /// Rate limited; the Unix time it ends when X said.
    RateLimited(Option<DateTime<Utc>>),
    /// The person cannot be messaged (closed DMs, not following, blocked), or the call is not
    /// allowed for this account. X's own words, links cut.
    NotAllowed(String),
    /// The token lacks a scope the call needs: connect the account again.
    Scope,
    /// Any other refusal, with X's reason.
    Refused(String),
    /// X may have done it: a 5xx, or an answer with no id. Never given back, never retried.
    Maybe(String),
    /// X could not be reached.
    Unreachable,
}

impl ApiError {
    /// What to tell a person; never a URL, token or secret.
    pub fn message(&self) -> String {
        match self {
            Self::Reconnect => "X refused the account's token. Connect the account again.".into(),
            Self::Credits => super::CREDITS.into(),
            Self::UsageCap => super::USAGE_CAPPED.into(),
            Self::RateLimited(Some(at)) => format!(
                "X rate limit reached; it resets at {} UTC.",
                at.format("%Y-%m-%d %H:%M")
            ),
            Self::RateLimited(None) => "X rate limit reached.".into(),
            Self::NotAllowed(why) | Self::Refused(why) | Self::Maybe(why) => why.clone(),
            Self::Scope => {
                "The X account was connected without the permission this needs. Connect it again."
                    .into()
            }
            Self::Unreachable => "X could not be reached.".into(),
        }
    }
}

/// What the scopes a token was granted allow in the inbox.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Granted {
    pub dms: bool,
    pub like: bool,
    pub hide: bool,
}

impl Granted {
    /// From X's space- or comma-separated `scope`; `None` (an account connected before the
    /// inbox scopes were asked for) grants nothing.
    pub fn of(scope: Option<&str>) -> Self {
        let have: Vec<&str> = scope.unwrap_or_default().split([' ', ',']).collect();
        let all = |want: &[&str]| want.iter().all(|w| have.contains(w));
        Self {
            dms: all(&["dm.read", "dm.write"]),
            like: all(&["like.write"]),
            hide: all(&["tweet.moderate.write"]),
        }
    }
}

/// X's problem types that mean "this person cannot take your message" (`dm-recipient-...`,
/// `recipient-not-messageable`, `dm-sender-...`).
fn dm_refused(body: &Value) -> bool {
    let kind = body["type"].as_str().unwrap_or_default();
    kind.contains("/dm-") || kind.contains("recipient-not-messageable")
}

fn classify(status: StatusCode, headers: &HeaderMap, body: &Value) -> ApiError {
    if status == StatusCode::UNAUTHORIZED {
        return ApiError::Reconnect;
    }
    if out_of_credits(status, body) {
        return ApiError::Credits;
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        if body["type"]
            .as_str()
            .is_some_and(|t| t.ends_with("usage-capped"))
        {
            return ApiError::UsageCap;
        }
        let reset = headers
            .get("x-rate-limit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok())
            .and_then(|s| DateTime::from_timestamp(s, 0));
        return ApiError::RateLimited(reset);
    }
    if status == StatusCode::FORBIDDEN {
        if dm_refused(body) {
            return ApiError::NotAllowed(reason(body, status));
        }
        let said = format!("{} {}", body["title"], body["detail"]).to_lowercase();
        if said.contains("scope") || said.contains("unsupported authentication") {
            return ApiError::Scope;
        }
    }
    if status.is_server_error() {
        return ApiError::Maybe(format!("X had an error ({}).", status.as_u16()));
    }
    ApiError::Refused(reason(body, status))
}

/// A person as X's `users` expansion gives them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Person {
    pub id: String,
    pub username: Option<String>,
    pub name: Option<String>,
    pub avatar: Option<String>,
}

/// One post that mentions the account, or replies to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    pub id: String,
    pub text: String,
    pub at: DateTime<FixedOffset>,
    pub author: Person,
    /// The conversation's first post.
    pub conversation_id: Option<String>,
    /// The post this one replies to.
    pub replied_to: Option<String>,
    /// The author of the post this one replies to.
    pub in_reply_to_user: Option<String>,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Default)]
pub struct Mentions {
    /// Newest first, as X sends them.
    pub items: Vec<Mention>,
    pub newest_id: Option<String>,
    pub next_token: Option<String>,
    /// Posts and users X returned, which is what it bills.
    pub posts: usize,
    pub users: usize,
}

impl Mentions {
    pub fn cost_micros(&self) -> i64 {
        self.posts as i64 * POST_READ_MICROS + self.users as i64 * USER_READ_MICROS
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmEvent {
    pub id: String,
    pub text: String,
    pub at: DateTime<FixedOffset>,
    pub sender: Person,
    pub conversation_id: String,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Default)]
pub struct DmEvents {
    /// Newest first.
    pub items: Vec<DmEvent>,
    pub next_token: Option<String>,
    pub events: usize,
    pub users: usize,
}

impl DmEvents {
    pub fn cost_micros(&self) -> i64 {
        self.events as i64 * DM_READ_MICROS + self.users as i64 * USER_READ_MICROS
    }
}

/// Where a DM goes: a person (X opens or finds the one-to-one conversation) or a conversation.
pub enum DmTo<'a> {
    Person(&'a str),
    Conversation(&'a str),
}

/// The `users` of a response by id.
fn people(body: &Value) -> Vec<Person> {
    let text = |u: &Value, k: &str| u[k].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
    body["includes"]["users"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| {
            Some(Person {
                id: plain_id(u["id"].as_str()?)?.to_owned(),
                username: text(u, "username"),
                name: text(u, "name"),
                avatar: text(u, "profile_image_url"),
            })
        })
        .collect()
}

/// The media of a response, by key: a photo is its file, a video or GIF the still X gives.
fn media(body: &Value, keys: &Value) -> Vec<Attachment> {
    let keys: Vec<&str> = keys
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    body["includes"]["media"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["media_key"].as_str().is_some_and(|k| keys.contains(&k)))
        .filter_map(|m| {
            let kind = match m["type"].as_str()? {
                "photo" => "image",
                "video" => "video",
                "animated_gif" => "gif",
                _ => return None,
            };
            let still = m["preview_image_url"].as_str();
            Attachment::new(kind, m["url"].as_str().or(still), still)
        })
        .take(crate::inbox::MAX_ATTACHMENTS)
        .collect()
}

fn time(v: &Value) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(v.as_str()?).ok()
}

fn id(v: &Value) -> Option<String> {
    plain_id(v.as_str()?).map(str::to_owned)
}

/// Parses `GET /2/users/:id/mentions`. A post without a known author is dropped.
fn parse_mentions(body: &Value) -> Mentions {
    let users = people(body);
    let items = body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let author = users
                .iter()
                .find(|u| Some(&u.id) == id(&t["author_id"]).as_ref())?;
            let replied_to = t["referenced_tweets"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| r["type"] == "replied_to")
                .and_then(|r| id(&r["id"]));
            Some(Mention {
                id: id(&t["id"])?,
                text: t["text"].as_str()?.to_owned(),
                at: time(&t["created_at"])?,
                author: author.clone(),
                conversation_id: id(&t["conversation_id"]),
                replied_to,
                in_reply_to_user: id(&t["in_reply_to_user_id"]),
                attachments: media(body, &t["attachments"]["media_keys"]),
            })
        })
        .collect::<Vec<_>>();
    Mentions {
        newest_id: id(&body["meta"]["newest_id"]),
        next_token: body["meta"]["next_token"].as_str().map(str::to_owned),
        posts: body["data"].as_array().map_or(0, Vec::len),
        users: users.len(),
        items,
    }
}

/// Parses `GET /2/dm_events`: message events only, with a known sender.
fn parse_dms(body: &Value) -> DmEvents {
    let users = people(body);
    let items = body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["event_type"] == "MessageCreate")
        .filter_map(|e| {
            let sender = users
                .iter()
                .find(|u| Some(&u.id) == id(&e["sender_id"]).as_ref())?;
            Some(DmEvent {
                id: id(&e["id"])?,
                text: e["text"].as_str().unwrap_or_default().to_owned(),
                at: time(&e["created_at"])?,
                sender: sender.clone(),
                conversation_id: e["dm_conversation_id"].as_str()?.to_owned(),
                attachments: media(body, &e["attachments"]["media_keys"]),
            })
        })
        .collect::<Vec<_>>();
    DmEvents {
        next_token: body["meta"]["next_token"].as_str().map(str::to_owned),
        events: body["data"].as_array().map_or(0, Vec::len),
        users: users.len(),
        items,
    }
}

impl X {
    async fn read(&self, req: reqwest::RequestBuilder, step: &str) -> Result<Value, ApiError> {
        let (status, headers, body) = self
            .call(req, step)
            .await
            .map_err(|_| ApiError::Unreachable)?;
        if status.is_success() {
            Ok(body)
        } else {
            Err(classify(status, &headers, &body))
        }
    }

    /// Mentions of the user, and replies to their posts (a reply mentions its author): newest
    /// first, only posts after `since_id` when given, else after `start` (a first read goes back
    /// a few days, not to the beginning). `max` is X's page size, 5 to 100. Needs `tweet.read`
    /// and `users.read`.
    pub async fn mentions(
        &self,
        token: &str,
        user_id: &str,
        (since_id, start): (Option<&str>, Option<DateTime<Utc>>),
        max: u32,
        page: Option<&str>,
    ) -> Result<Mentions, ApiError> {
        let Some(user_id) = plain_id(user_id) else {
            return Err(ApiError::Refused("This is not an X user id.".into()));
        };
        let mut q: Vec<(&str, String)> = vec![
            ("max_results", max.clamp(5, 100).to_string()),
            (
                "tweet.fields",
                "created_at,author_id,conversation_id,in_reply_to_user_id,referenced_tweets,attachments".into(),
            ),
            ("expansions", "author_id,attachments.media_keys".into()),
            ("user.fields", "username,name,profile_image_url".into()),
            ("media.fields", "url,preview_image_url,type".into()),
        ];
        if let Some(s) = since_id.and_then(plain_id) {
            q.push(("since_id", s.into()));
        } else if let Some(t) = start {
            q.push(("start_time", t.format("%Y-%m-%dT%H:%M:%SZ").to_string()));
        }
        if let Some(p) = page {
            q.push(("pagination_token", p.into()));
        }
        let req = self
            .api(Method::GET, &format!("/2/users/{user_id}/mentions"), token)
            .query(&q);
        self.read(req, "Mentions").await.map(|b| parse_mentions(&b))
    }

    /// The newest direct message events across the user's conversations (`dm.read`). X has no
    /// `since_id` here, so callers ask for a few and stop at an event they already hold.
    pub async fn dm_events(
        &self,
        token: &str,
        max: u32,
        page: Option<&str>,
    ) -> Result<DmEvents, ApiError> {
        let mut q: Vec<(&str, String)> = vec![
            ("max_results", max.clamp(1, 100).to_string()),
            ("event_types", "MessageCreate".into()),
            (
                "dm_event.fields",
                "created_at,sender_id,dm_conversation_id,attachments,event_type".into(),
            ),
            ("expansions", "sender_id,attachments.media_keys".into()),
            ("user.fields", "username,name,profile_image_url".into()),
            ("media.fields", "url,preview_image_url,type".into()),
        ];
        if let Some(p) = page {
            q.push(("pagination_token", p.into()));
        }
        let req = self.api(Method::GET, "/2/dm_events", token).query(&q);
        self.read(req, "Messages").await.map(|b| parse_dms(&b))
    }

    /// Sends a DM (`dm.write`); the new event's id. X refuses a person who does not follow the
    /// account and has DMs closed (`NotAllowed`).
    pub async fn send_dm(&self, token: &str, to: DmTo<'_>, text: &str) -> Result<String, ApiError> {
        let path = match to {
            DmTo::Person(id) => format!(
                "/2/dm_conversations/with/{}/messages",
                plain_id(id).ok_or(ApiError::Refused("This is not an X user id.".into()))?
            ),
            DmTo::Conversation(id)
                if id.len() <= 80 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') =>
            {
                format!("/2/dm_conversations/{id}/messages")
            }
            DmTo::Conversation(_) => {
                return Err(ApiError::Refused(
                    "This is not an X conversation id.".into(),
                ))
            }
        };
        let req = self
            .api(Method::POST, &path, token)
            .json(&json!({"text": text}));
        let body = self.read(req, "Message").await?;
        id(&body["data"]["dm_event_id"])
            .ok_or_else(|| ApiError::Maybe("X sent no message id.".into()))
    }

    /// A public reply to `to` (`tweet.write`); the new post's id.
    pub async fn reply(&self, token: &str, text: &str, to: &str) -> Result<String, ApiError> {
        let Some(to) = plain_id(to) else {
            return Err(ApiError::Refused("This is not an X post id.".into()));
        };
        let body = json!({"text": text, "reply": {"in_reply_to_tweet_id": to}});
        let req = self.api(Method::POST, "/2/tweets", token).json(&body);
        let answer = self.read(req, "Reply").await?;
        id(&answer["data"]["id"]).ok_or_else(|| ApiError::Maybe("X sent no post id.".into()))
    }

    /// Likes (`like.write`) or reposts (`tweet.write`) post `tweet` as user `me`.
    pub async fn react(
        &self,
        token: &str,
        me: &str,
        tweet: &str,
        repost: bool,
    ) -> Result<(), ApiError> {
        let (Some(me), Some(tweet)) = (plain_id(me), plain_id(tweet)) else {
            return Err(ApiError::Refused("This is not an X id.".into()));
        };
        let path = format!(
            "/2/users/{me}/{}",
            if repost { "retweets" } else { "likes" }
        );
        let req = self
            .api(Method::POST, &path, token)
            .json(&json!({"tweet_id": tweet}));
        self.read(req, "React").await.map(|_| ())
    }

    /// Hides (or shows again) a reply to one of the user's posts (`tweet.moderate.write`).
    pub async fn hide(&self, token: &str, tweet: &str, hidden: bool) -> Result<(), ApiError> {
        let Some(tweet) = plain_id(tweet) else {
            return Err(ApiError::Refused("This is not an X post id.".into()));
        };
        let req = self
            .api(Method::PUT, &format!("/2/tweets/{tweet}/hidden"), token)
            .json(&json!({"hidden": hidden}));
        self.read(req, "Hide").await.map(|_| ())
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
    fn scopes_say_what_the_inbox_may_do() {
        let g = Granted::of(Some("tweet.read dm.read dm.write like.write"));
        assert_eq!(
            g,
            Granted {
                dms: true,
                like: true,
                hide: false
            }
        );
        assert_eq!(Granted::of(Some("dm.read")), Granted::default());
        assert_eq!(Granted::of(None), Granted::default());
        assert!(Granted::of(Some("tweet.moderate.write,dm.write,dm.read")).hide);
    }

    #[test]
    fn mentions_are_parsed_with_author_and_context() {
        let body = json!({
            "data": [
                {"id": "20", "text": "@me nice", "author_id": "7", "created_at": "2026-11-20T10:00:00.000Z",
                 "conversation_id": "10", "in_reply_to_user_id": "1",
                 "referenced_tweets": [{"type": "replied_to", "id": "10"}],
                 "attachments": {"media_keys": ["m1", "m9"]}},
                {"id": "21", "text": "no author", "author_id": "99", "created_at": "2026-11-20T10:01:00.000Z"}
            ],
            "includes": {
                "users": [{"id": "7", "username": "ann", "name": "Ann", "profile_image_url": "https://pbs.example/a.jpg"}],
                "media": [{"media_key": "m1", "type": "photo", "url": "https://pbs.example/p.jpg"},
                          {"media_key": "m2", "type": "photo", "url": "https://pbs.example/other.jpg"}]
            },
            "meta": {"newest_id": "21", "next_token": "abc", "result_count": 2}
        });
        let got = parse_mentions(&body);
        assert_eq!(got.items.len(), 1, "a post without its author is dropped");
        let m = &got.items[0];
        assert_eq!(
            (m.id.as_str(), m.author.username.as_deref()),
            ("20", Some("ann"))
        );
        assert_eq!(m.replied_to.as_deref(), Some("10"));
        assert_eq!(m.in_reply_to_user.as_deref(), Some("1"));
        assert_eq!(m.attachments.len(), 1);
        assert_eq!(got.newest_id.as_deref(), Some("21"));
        assert_eq!(got.next_token.as_deref(), Some("abc"));
        // Two posts and one user were returned, and are billed.
        assert_eq!((got.posts, got.users), (2, 1));
        assert_eq!(got.cost_micros(), 2 * 5_000 + 10_000);
    }

    #[test]
    fn dm_events_keep_messages_with_a_sender() {
        let body = json!({
            "data": [
                {"id": "5", "event_type": "MessageCreate", "text": "hi", "sender_id": "7",
                 "dm_conversation_id": "7-1", "created_at": "2026-11-20T10:00:00.000Z"},
                {"id": "6", "event_type": "ParticipantsJoin", "dm_conversation_id": "7-1",
                 "created_at": "2026-11-20T10:00:00.000Z"}
            ],
            "includes": {"users": [{"id": "7", "username": "ann"}]},
            "meta": {"result_count": 2}
        });
        let got = parse_dms(&body);
        assert_eq!(got.items.len(), 1);
        assert_eq!(got.items[0].conversation_id, "7-1");
        assert_eq!((got.events, got.users), (2, 1));
        assert_eq!(got.cost_micros(), 30_000);
    }

    #[test]
    fn refusals_are_classified() {
        let none = Value::Null;
        let c = |s: u16, b: &Value, h: &HeaderMap| classify(StatusCode::from_u16(s).unwrap(), h, b);
        assert_eq!(c(401, &none, &headers(None)), ApiError::Reconnect);
        assert_eq!(c(402, &none, &headers(None)), ApiError::Credits);
        assert_eq!(
            c(403, &json!({"title": "CreditsDepleted"}), &headers(None)),
            ApiError::Credits
        );
        let capped = json!({"type": "https://api.x.com/2/problems/usage-capped"});
        assert_eq!(c(429, &capped, &headers(None)), ApiError::UsageCap);
        assert_eq!(
            c(429, &none, &headers(Some("1793795400"))),
            ApiError::RateLimited(DateTime::from_timestamp(1793795400, 0))
        );
        let dm = json!({"type": "https://api.x.com/2/problems/dm-recipient-not-accepting", "detail": "Closed."});
        assert_eq!(
            c(403, &dm, &headers(None)),
            ApiError::NotAllowed("Closed.".into())
        );
        let scope = json!({"title": "Forbidden", "detail": "Missing required scope dm.read"});
        assert_eq!(c(403, &scope, &headers(None)), ApiError::Scope);
        assert!(matches!(c(500, &none, &headers(None)), ApiError::Maybe(_)));
        assert!(matches!(
            c(400, &none, &headers(None)),
            ApiError::Refused(_)
        ));
        // A 403 about something else than a DM is X's own reason, not "the recipient said no".
        let dup = json!({"detail": "Duplicate content."});
        assert_eq!(
            c(403, &dup, &headers(None)),
            ApiError::Refused("Duplicate content.".into())
        );
        assert!(ApiError::Credits.message().contains("credits"));
    }
}
