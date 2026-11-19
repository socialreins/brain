//! RSS and Atom feeds that post on their own. `Fetcher` reads a feed the way `media.rs` downloads
//! a file: a public https URL only, names resolved by `PublicOnly`, at most 3 redirects each
//! checked again, 10 seconds and 1 MB at most. `tick` (every 15 minutes, one instance at a time)
//! checks the active feeds; `check` posts a feed's new items, at most 5 a check and oldest first,
//! each as one post group saved together with the mark that it was seen, so none posts twice.
use std::{collections::HashSet, sync::Arc, time::Duration};

use axum::{body::to_bytes, http::StatusCode, response::Response};
use chrono::{DateTime, Utc};
use reqwest::{
    header::{ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED},
    Url,
};
use sea_orm::{
    sea_query::{NullOrdering, OnConflict},
    ActiveValue::Set,
    ColumnTrait, Condition, ConnectionTrait, DbBackend, DbErr, EntityTrait, Order, QueryFilter,
    QueryOrder, QuerySelect, Statement, TransactionTrait, Value,
};
use serde_json::json;
use socialreins_core::{post, team::Role, token};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

use crate::{
    connectors::bluesky::PublicOnly,
    controllers::{api_tokens::Actor, posts},
    media::{blocked, MAX_REDIRECTS},
    models::{feed_items, feeds, social_sets, teams},
    state::AppState,
    webhooks::public_url,
};

/// How often the scheduler checks an active feed, in seconds.
pub const EVERY_SECS: i64 = 15 * 60;
/// The least time between two checks of one feed, the manual ones included, in seconds.
pub const CHECK_NOW_SECS: i64 = 60;
/// New items posted per check; the rest wait for the next one.
pub const PER_CHECK: usize = 5;
/// Refused creates after which an item is given up (marked seen).
pub const MAX_TRIES: i32 = 3;
/// Failed checks in a row after which a feed is switched off.
pub const MAX_FAILURES: i32 = 10;
/// Characters of an item's description `{summary}` stands for.
pub const SUMMARY_CHARS: usize = 300;
const MAX_BYTES: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);
const FEEDS_LOCK: i64 = 0x0046_4545_4453_5353;

/// The client that reads feeds.
#[derive(Clone)]
pub struct Fetcher {
    client: reqwest::Client,
    test_origin: Option<String>,
}

/// What a feed answered.
pub enum Fetched {
    /// `304`: nothing changed since the `ETag` or `Last-Modified` sent.
    NotModified,
    Body {
        bytes: Vec<u8>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

fn safe(url: &Url, test_origin: Option<&str>) -> bool {
    public_url(url, false) || test_origin.is_some_and(|o| url.origin().ascii_serialization() == o)
}

impl Fetcher {
    /// `test_origin` (tests only, never read from the environment) is the one origin, such as
    /// `http://127.0.0.1:4010`, a feed may also be read from: the tests' feed server. Every
    /// other URL, a redirect's too, passes `public_url` as in production.
    pub fn new(test_origin: Option<String>) -> Self {
        let allowed = test_origin.clone();
        let redirects = reqwest::redirect::Policy::custom(move |a| {
            if a.previous().len() > MAX_REDIRECTS {
                a.error("too many redirects")
            } else if safe(a.url(), allowed.as_deref()) {
                a.follow()
            } else {
                a.error("redirect to a URL that is not public")
            }
        });
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            // The whole exchange, the body included.
            .timeout(TIMEOUT)
            .redirect(redirects)
            .dns_resolver(Arc::new(PublicOnly))
            .user_agent("SocialReins")
            .build()
            .expect("the feed http client builds");
        Self {
            client,
            test_origin,
        }
    }

    /// Whether `url` may be saved as a feed: `https://`, no credentials, a public host.
    pub fn allows(&self, url: &str) -> bool {
        Url::parse(url)
            .ok()
            .is_some_and(|u| safe(&u, self.test_origin.as_deref()))
    }

    /// The feed at `url`, asked with the validators it gave last time. The error is a short
    /// reason fit for `last_error`.
    pub async fn get(
        &self,
        url: &str,
        etag: Option<&str>,
        modified: Option<&str>,
    ) -> Result<Fetched, String> {
        if !self.allows(url) {
            return Err("not a public https URL".into());
        }
        let mut req = self.client.get(url);
        if let Some(e) = etag {
            req = req.header(IF_NONE_MATCH, e);
        }
        if let Some(m) = modified {
            req = req.header(IF_MODIFIED_SINCE, m);
        }
        let reason = |e: reqwest::Error| -> String {
            if e.is_timeout() {
                "timed out".into()
            } else if e.is_redirect() {
                "too many redirects, or one to a URL that is not public".into()
            } else if blocked(&e) {
                "the host has no public address".into()
            } else {
                "unreachable".into()
            }
        };
        let mut res = req.send().await.map_err(reason)?;
        if res.status() == StatusCode::NOT_MODIFIED {
            return Ok(Fetched::NotModified);
        }
        if !res.status().is_success() {
            return Err(format!("HTTP {}", res.status().as_u16()));
        }
        let header = |name| {
            let v = res.headers().get(name).and_then(|v| v.to_str().ok());
            v.map(str::to_owned)
        };
        let (etag, last_modified) = (header(ETAG), header(LAST_MODIFIED));
        let too_large = || "over 1 MB".to_owned();
        if res.content_length().is_some_and(|n| n > MAX_BYTES as u64) {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = res.chunk().await.map_err(reason)? {
            if bytes.len() + chunk.len() > MAX_BYTES {
                return Err(too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(Fetched::Body {
            bytes,
            etag,
            last_modified,
        })
    }

    /// The items of the feed at `url`, fetched without validators.
    pub async fn items(&self, url: &str) -> Result<Vec<Item>, String> {
        match self.get(url, None, None).await? {
            Fetched::Body { bytes, .. } => parsed(bytes).await,
            Fetched::NotModified => Err("HTTP 304".into()),
        }
    }
}

/// One item of a feed.
pub struct Item {
    /// `token::hash` of its guid, else its link, else its title.
    pub hash: String,
    pub title: String,
    pub link: String,
    /// The description as plain text, at most `SUMMARY_CHARS` characters.
    pub summary: String,
    /// Its image enclosure or `media:content` image, when that is an https link.
    pub image: Option<String>,
    pub date: Option<DateTime<Utc>>,
}

/// `html` as plain text on one line: tags dropped, the common entities decoded.
/// shortcut: not an HTML parser, so a bare `<` in text drops what follows up to the next `>`;
/// use one if summaries come out cut.
pub fn plain(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut tag = false;
    for c in html.chars() {
        match c {
            '<' => tag = true,
            '>' if tag => {
                tag = false;
                out.push(' ');
            }
            _ if !tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn image_of(e: &feed_rs::model::Entry) -> Option<String> {
    let typed = |url: &str, ty: Option<&str>| match ty {
        Some(t) => t.starts_with("image/"),
        None => {
            let path = url.split(['?', '#']).next().unwrap_or_default();
            let ext = path.rsplit('.').next().unwrap_or_default();
            ["jpg", "jpeg", "png", "webp", "gif"].contains(&ext.to_ascii_lowercase().as_str())
        }
    };
    let media = e.media.iter().flat_map(|m| &m.content).filter_map(|c| {
        let ty = c.content_type.as_ref().map(ToString::to_string);
        Some((c.url.as_ref()?.to_string(), ty))
    });
    let links = e
        .links
        .iter()
        .filter(|l| l.rel.as_deref() == Some("enclosure"))
        .map(|l| (l.href.clone(), l.media_type.clone()));
    media
        .chain(links)
        .find(|(url, ty)| post::https_url(url) && typed(url, ty.as_deref()))
        .map(|(url, _)| url)
}

/// What a check fails with when the parser itself broke on a feed.
pub const UNREADABLE: &str = "The feed could not be read";

/// `work` on the blocking pool; a panic in it is `UNREADABLE`, not the end of the caller's task.
async fn guarded<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let done = tokio::task::spawn_blocking(work).await;
    done.unwrap_or_else(|_| Err(UNREADABLE.to_owned()))
}

/// `parse` off the async threads: a feed is someone else's megabyte of XML, and a parser bug
/// it sets off must cost one failed check, never the scheduler's feed task.
pub async fn parsed(bytes: Vec<u8>) -> Result<Vec<Item>, String> {
    guarded(move || parse(&bytes)).await
}

/// `href` when it is an absolute `http` or `https` URL: the only links a post may carry, so a
/// feed cannot put `javascript:`, `data:`, `ftp:` or a relative link in one.
fn web(href: &str) -> Option<String> {
    let url = Url::parse(href.trim()).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

/// The page among `links` (not a file or the comments), when it is a web link.
fn page_of(links: &[feed_rs::model::Link]) -> Option<String> {
    let page = links.iter().find(|l| {
        let rel = l.rel.as_deref();
        rel.is_none() || rel == Some("alternate")
    });
    page.or(links.first()).and_then(|l| web(&l.href))
}

/// The items of an RSS or Atom document, in its order, each guid once.
pub fn parse(bytes: &[u8]) -> Result<Vec<Item>, String> {
    let parser = feed_rs::parser::Builder::new()
        // An item without a guid is known by its link.
        .id_generator(|links, _, _| links.first().map(|l| l.href.clone()).unwrap_or_default())
        .build();
    let feed = parser
        .parse(bytes)
        .map_err(|_| "not an RSS or Atom feed".to_owned())?;
    // What an item without a usable link of its own points to.
    let site = page_of(&feed.links).unwrap_or_default();
    let mut known = HashSet::new();
    let mut items = Vec::new();
    for e in feed.entries {
        let image = image_of(&e);
        let title = e.title.map(|t| plain(&t.content)).unwrap_or_default();
        // As written, for the hash alone: what an item is known by is never posted.
        let raw = e.links.first().map(|l| l.href.as_str()).unwrap_or_default();
        let link = page_of(&e.links).unwrap_or_else(|| site.clone());
        let guid = [e.id.as_str(), raw, &title]
            .into_iter()
            .find(|s| !s.is_empty());
        let Some(guid) = guid else {
            continue;
        };
        let hash = token::hash(guid);
        if !known.insert(hash.clone()) {
            continue;
        }
        let described = e.summary.map(|t| t.content);
        let described = described.or(e.content.and_then(|c| c.body));
        let summary = plain(&described.unwrap_or_default());
        items.push(Item {
            hash,
            title,
            link,
            summary: summary.chars().take(SUMMARY_CHARS).collect(),
            image,
            date: e.published.or(e.updated),
        });
    }
    Ok(items)
}

/// Whether `body` is within `platform`'s text limit (X's 280, whatever plan the account has).
fn fits(platform: &str, body: &str) -> bool {
    let long = post::check(platform, body, &[])
        .iter()
        .any(|p| matches!(p.invalid, post::Invalid::TooLong));
    !long && (platform != "x" || post::check_x_short([body]).is_none())
}

/// `template` with `{title}`, `{summary}` and `{link}` filled in for `platform`. Over the
/// platform's limit the summary is cut first, then the title, each ending in `…`; the link and
/// the template's own text are never cut, so a body may still be too long with both gone.
pub fn render(template: &str, platform: &str, item: &Item) -> String {
    let fill = |title: &str, summary: &str| {
        template
            .replace("{title}", title)
            .replace("{summary}", summary)
            .replace("{link}", &item.link)
    };
    let full = fill(&item.title, &item.summary);
    if fits(platform, &full) {
        return full;
    }
    let title: Vec<&str> = item.title.graphemes(true).collect();
    let summary: Vec<&str> = item.summary.graphemes(true).collect();
    let cut = |part: &[&str], keep: usize| match keep {
        0 => String::new(),
        k if k >= part.len() => part.concat(),
        k => format!("{}…", part[..k].concat().trim_end()),
    };
    // `kept` graphemes of the two together, the title's first.
    let at = |kept: usize| {
        let of_title = kept.min(title.len());
        fill(&cut(&title, of_title), &cut(&summary, kept - of_title))
    };
    let (mut low, mut high) = (0, title.len() + summary.len());
    while low < high {
        let mid = (low + high).div_ceil(2);
        if fits(platform, &at(mid)) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    at(low)
}

fn sql(text: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, text, values)
}

/// Marks every item seen for `feed`: what a new feed (or a new URL) holds is never posted.
pub async fn seed(db: &impl ConnectionTrait, feed: Uuid, items: &[Item]) -> Result<(), DbErr> {
    // In parts: one statement takes 65535 values at most.
    for part in items.chunks(1000) {
        let rows = part.iter().map(|i| feed_items::ActiveModel {
            feed_id: Set(feed),
            guid_hash: Set(i.hash.clone()),
            seen: Set(true),
            ..Default::default()
        });
        let again = OnConflict::columns([feed_items::Column::FeedId, feed_items::Column::GuidHash])
            .update_column(feed_items::Column::Seen)
            .to_owned();
        feed_items::Entity::insert_many(rows)
            .on_conflict(again)
            .exec_without_returning(db)
            .await?;
    }
    Ok(())
}

/// The feed, with `last_checked_at` moved to now, when its last check is `every` seconds old
/// (and, with `active`, it is switched on); `None` otherwise. One statement, so of two callers
/// at once one gets it.
pub async fn claim(
    db: &impl ConnectionTrait,
    id: Uuid,
    every: i64,
    active: bool,
) -> Result<Option<feeds::Model>, DbErr> {
    let text = "UPDATE feeds SET last_checked_at = now() WHERE id = $1 AND (active OR NOT $3) \
        AND (last_checked_at IS NULL \
          OR last_checked_at <= now() - make_interval(secs => $2::double precision)) \
        RETURNING *";
    let values = vec![id.into(), (every as f64).into(), active.into()];
    feeds::Entity::find()
        .from_raw_sql(sql(text, values))
        .one(db)
        .await
}

/// A failed check: its reason, and the feed switched off at `MAX_FAILURES` in a row.
async fn failed(db: &impl ConnectionTrait, id: Uuid, reason: &str) -> Result<(), DbErr> {
    let text = "UPDATE feeds SET failures = failures + 1, last_error = $2, \
        active = active AND failures + 1 < $3 WHERE id = $1";
    let values = vec![id.into(), reason.into(), MAX_FAILURES.into()];
    db.execute_raw(sql(text, values)).await.map(|_| ())
}

enum Outcome {
    Posted,
    /// The queue had no slot, so the group is a draft.
    Draft,
    /// Nothing to post (Instagram alone, no image), or another check posted it meanwhile.
    Nothing,
    Failed(String),
}

async fn error_code(res: Response) -> String {
    let status = res.status();
    let body = to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap_or_default();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    match v["error"].as_str() {
        Some(code) => code.to_owned(),
        None => format!("HTTP {}", status.as_u16()),
    }
}

/// Creates the post group of one new item as `a`, marking the item seen in the group's own
/// transaction (the guard of `posts::create_group_guarded`).
async fn post_item(
    app: &AppState,
    a: &Actor,
    feed: &feeds::Model,
    item: &Item,
) -> Result<Outcome, DbErr> {
    let all: Vec<String> = serde_json::from_value(feed.platforms.clone()).unwrap_or_default();
    // Instagram takes no post without media.
    let platforms: Vec<&String> = all
        .iter()
        .filter(|p| *p != "instagram" || item.image.is_some())
        .collect();
    let seen = "INSERT INTO feed_items (feed_id, guid_hash) VALUES ($1, $2) \
        ON CONFLICT (feed_id, guid_hash) DO UPDATE SET seen = true WHERE NOT feed_items.seen";
    let key = || vec![feed.id.into(), item.hash.as_str().into()];
    if platforms.is_empty() {
        app.db.execute_raw(sql(seen, key())).await?;
        return Ok(Outcome::Nothing);
    }
    let overrides: serde_json::Map<String, serde_json::Value> = platforms
        .iter()
        .map(|p| {
            let body = render(&feed.template, p, item);
            ((*p).clone(), json!({ "body": body }))
        })
        .collect();
    let input = |mode: &str| {
        let mut v = json!({"social_set_id": feed.social_set_id, "platforms": platforms,
            "media": item.image.iter().collect::<Vec<_>>(), "overrides": overrides,
            // A platform that cannot take the item is left out, the others still post.
            "on_invalid": "skip"});
        match mode {
            "queue" => v["queue"] = json!(true),
            "now" => v["scheduled_at"] = json!(Utc::now().to_rfc3339()),
            _ => {}
        }
        serde_json::from_value::<posts::PostGroupInput>(v)
    };
    // The mark and the feed's count, only when the mark is new: no row changes otherwise.
    let guard = format!(
        "WITH marked AS ({seen} RETURNING 1) UPDATE feeds SET items_posted = items_posted + 1 \
         WHERE id = $1 AND EXISTS (SELECT 1 FROM marked)"
    );
    let create = |mode: &'static str| async move {
        let Ok(input) = input(mode) else {
            return Err("internal".to_owned());
        };
        let guard = Some(sql(&guard, key()));
        let res = posts::create_group_guarded(app, a, input, false, guard).await;
        if res.status() == StatusCode::CREATED {
            Ok(())
        } else {
            Err(error_code(res).await)
        }
    };
    let mode: &'static str = match feed.mode.as_str() {
        "queue" => "queue",
        "now" => "now",
        _ => "draft",
    };
    let mut outcome = Outcome::Posted;
    let mut made = create.clone()(mode).await;
    if made.as_ref().is_err_and(|code| code == "queue_empty") {
        outcome = Outcome::Draft;
        made = create("draft").await;
    }
    match made {
        Ok(()) => Ok(outcome),
        Err(code) if code == "guard_failed" => Ok(Outcome::Nothing),
        Err(code) => {
            let again = "INSERT INTO feed_items (feed_id, guid_hash, failed, seen) \
                VALUES ($1, $2, 1, false) ON CONFLICT (feed_id, guid_hash) DO UPDATE \
                SET failed = feed_items.failed + 1, \
                    seen = feed_items.seen OR feed_items.failed + 1 >= $3";
            let mut values = key();
            values.push(MAX_TRIES.into());
            app.db.execute_raw(sql(again, values)).await?;
            let title: String = item.title.chars().take(80).collect();
            Ok(Outcome::Failed(format!(
                "\"{title}\" was not posted: {code}"
            )))
        }
    }
}

/// Which of `items` the feed has seen.
async fn seen_of(
    db: &impl ConnectionTrait,
    feed: Uuid,
    items: &[Item],
) -> Result<HashSet<String>, DbErr> {
    let mut seen = HashSet::new();
    for part in items.chunks(10_000) {
        let hashes: Vec<String> = feed_items::Entity::find()
            .select_only()
            .column(feed_items::Column::GuidHash)
            .filter(feed_items::Column::FeedId.eq(feed))
            .filter(feed_items::Column::Seen.eq(true))
            .filter(feed_items::Column::GuidHash.is_in(part.iter().map(|i| i.hash.as_str())))
            .into_tuple()
            .all(db)
            .await?;
        seen.extend(hashes);
    }
    Ok(seen)
}

/// One check of a claimed feed (`claim`): reads it and posts its new items, at most `PER_CHECK`
/// and oldest first. A feed that cannot be read or parsed is a failed check; a refused item only
/// sets `last_error` and is tried again by the next check, `MAX_TRIES` times in all.
pub async fn check(app: &AppState, feed: feeds::Model) -> Result<(), DbErr> {
    let db = &app.db;
    let fetched = app
        .feeds
        .get(
            &feed.url,
            feed.etag.as_deref(),
            feed.last_modified.as_deref(),
        )
        .await;
    let (bytes, etag, last_modified) = match fetched {
        Err(reason) => return failed(db, feed.id, &reason).await,
        Ok(Fetched::NotModified) => {
            let text = "UPDATE feeds SET failures = 0 WHERE id = $1";
            return db
                .execute_raw(sql(text, vec![feed.id.into()]))
                .await
                .map(|_| ());
        }
        Ok(Fetched::Body {
            bytes,
            etag,
            last_modified,
        }) => (bytes, etag, last_modified),
    };
    let items = match parsed(bytes).await {
        Ok(items) => items,
        Err(reason) => return failed(db, feed.id, &reason).await,
    };
    let set = social_sets::Entity::find_by_id(feed.social_set_id)
        .filter(social_sets::Column::DeletedAt.is_null())
        .one(db)
        .await?;
    let team = teams::Entity::find_by_id(feed.team_id).one(db).await?;
    let (Some(_), Some(team)) = (set, team) else {
        // Nothing is tried, so no item is used up while the set can still be restored.
        return failed(db, feed.id, "the social set was deleted").await;
    };
    let seen = seen_of(db, feed.id, &items).await?;
    let mut new: Vec<Item> = items
        .into_iter()
        .filter(|i| !seen.contains(&i.hash))
        .collect();
    // Feeds list the newest first; dated items go by their date.
    new.reverse();
    new.sort_by_key(|i| i.date);
    // A feed posts as a manager: no one is there to approve its posts.
    let actor = Actor {
        team,
        role: Role::Admin,
        user_id: feed.created_by,
        token: None,
    };
    let mut waiting = new.len() > PER_CHECK;
    let mut error = None;
    for item in new.iter().take(PER_CHECK) {
        match post_item(app, &actor, &feed, item).await? {
            Outcome::Posted | Outcome::Nothing => {}
            Outcome::Draft => {
                error = Some("The queue has no slots, so the post was saved as a draft.".into());
            }
            Outcome::Failed(reason) => {
                waiting = true;
                error = Some(reason);
            }
        }
    }
    // Items still waiting need the whole feed next time, not a `304`.
    let (etag, last_modified) = if waiting {
        (None, None)
    } else {
        (etag, last_modified)
    };
    let text = "UPDATE feeds SET failures = 0, last_error = $2, etag = $3, last_modified = $4 \
        WHERE id = $1";
    let values = vec![
        feed.id.into(),
        error.into(),
        etag.into(),
        last_modified.into(),
    ];
    db.execute_raw(sql(text, values)).await.map(|_| ())
}

/// Feeds checked at once in a round.
const AT_ONCE: usize = 8;
/// How long a round starts checks for; feeds it did not reach wait for the next tick.
const ROUND: Duration = Duration::from_secs(5 * 60);

/// One round: the active feeds not checked for `EVERY_SECS`, the longest unchecked first (new
/// ones before all), `AT_ONCE` at a time, so a slow feed holds up one place and not the round.
/// No check starts after `ROUND`: what is left is first in the next round. Skips while another
/// instance holds the lock.
///
/// The advisory lock is transaction-level, as in the other ticks, so this one transaction (one
/// pool connection, idle) stays open for the round, which `ROUND` plus the last checks bounds.
/// Nothing else waits on a feed with a connection in hand: `check` reads the feed before it
/// asks the pool for anything, and each write is its own short statement or transaction.
pub async fn tick(app: &AppState) -> Result<(), DbErr> {
    let lock = app.db.begin().await?;
    let row = lock
        .query_one_raw(sql(
            "SELECT pg_try_advisory_xact_lock($1) AS ok",
            vec![FEEDS_LOCK.into()],
        ))
        .await?;
    let locked: bool = match row {
        Some(r) => r.try_get("", "ok")?,
        None => false,
    };
    if !locked {
        return Ok(());
    }
    let before = (Utc::now() - chrono::Duration::seconds(EVERY_SECS)).fixed_offset();
    let due: Vec<Uuid> = feeds::Entity::find()
        .select_only()
        .column(feeds::Column::Id)
        .filter(feeds::Column::Active.eq(true))
        .filter(
            Condition::any()
                .add(feeds::Column::LastCheckedAt.is_null())
                .add(feeds::Column::LastCheckedAt.lte(before)),
        )
        .order_by_with_nulls(
            feeds::Column::LastCheckedAt,
            Order::Asc,
            NullOrdering::First,
        )
        .into_tuple()
        .all(&app.db)
        .await?;
    let until = tokio::time::Instant::now() + ROUND;
    let mut running = tokio::task::JoinSet::new();
    for id in due {
        if running.len() >= AT_ONCE {
            running.join_next().await;
        }
        if tokio::time::Instant::now() >= until {
            tracing::warn!("feed round out of time, the rest wait for the next");
            break;
        }
        let app = app.clone();
        running.spawn(async move {
            let done = async {
                match claim(&app.db, id, EVERY_SECS, true).await? {
                    Some(feed) => check(&app, feed).await,
                    None => Ok(()),
                }
            };
            if let Err(e) = done.await {
                tracing::warn!(error = %e, feed = %id, "feed check failed");
            }
        });
    }
    // A check that panicked ends its own task only; the others and the round go on.
    while running.join_next().await.is_some() {}
    lock.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str, summary: &str) -> Item {
        Item {
            hash: String::new(),
            title: title.into(),
            link: "https://blog.example/a-post".into(),
            summary: summary.into(),
            image: None,
            date: None,
        }
    }

    #[test]
    fn html_becomes_plain_text() {
        let html = "<p>Tea &amp; <b>cake</b>,&nbsp;4 &lt; 5</p>\n<img src=\"a.png\"> now";
        assert_eq!(plain(html), "Tea & cake , 4 < 5 now");
    }

    #[test]
    fn a_body_that_fits_is_left_whole() {
        let body = render("{title}\n{link}", "bluesky", &item("Hello", "ignored"));
        assert_eq!(body, "Hello\nhttps://blog.example/a-post");
    }

    #[test]
    fn the_summary_is_cut_before_the_title_and_the_link_never() {
        let it = item(&"t".repeat(100), &"s".repeat(300));
        let body = render("{title}\n{summary}\n{link}", "bluesky", &it);
        assert_eq!(body.graphemes(true).count(), 300);
        assert!(body.starts_with(&format!("{}\n", "t".repeat(100))));
        assert!(body.ends_with("…\nhttps://blog.example/a-post"));
        // With no room for any summary the title is cut too.
        let it = item(&"t".repeat(400), "summary");
        let body = render("{title}{summary} {link}", "x", &it);
        assert_eq!(post::x_weight(&body), 280);
        assert!(body.ends_with("t… https://blog.example/a-post"));
    }

    #[test]
    fn rss_and_atom_items_are_read() {
        let rss = br#"<rss version="2.0" xmlns:media="http://search.yahoo.com/mrss/"><channel>
            <title>Blog</title>
            <item><title>One</title><link>https://blog.example/1</link><guid>g1</guid>
              <description>&lt;p&gt;First &lt;b&gt;post&lt;/b&gt;&lt;/p&gt;</description>
              <enclosure url="https://blog.example/1.jpg" type="image/jpeg" length="1"/></item>
            <item><title>Two</title><link>https://blog.example/2</link>
              <media:content url="https://blog.example/2.png" medium="image"/></item>
            <item><title>Three</title><link>https://blog.example/3</link>
              <enclosure url="https://blog.example/3.mp3" type="audio/mpeg" length="1"/>
              <media:content url="http://blog.example/3.png" type="image/png"/></item>
            <item><title>One again</title><guid>g1</guid></item>
            </channel></rss>"#;
        let items = parse(rss).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].hash, token::hash("g1"));
        assert_eq!(items[0].summary, "First post");
        assert_eq!(
            items[0].image.as_deref(),
            Some("https://blog.example/1.jpg")
        );
        assert_eq!(items[1].hash, token::hash("https://blog.example/2"));
        assert_eq!(
            items[1].image.as_deref(),
            Some("https://blog.example/2.png")
        );
        // Audio is no image, and a plain http image is not taken.
        assert_eq!(items[2].image, None);

        let atom = br#"<feed xmlns="http://www.w3.org/2005/Atom"><title>Blog</title>
            <entry><id>urn:a</id><title>Atom one</title>
              <link rel="enclosure" type="image/png" href="https://blog.example/a.png"/>
              <link rel="alternate" href="https://blog.example/a"/>
              <summary>Short</summary><updated>2026-11-01T00:00:00Z</updated></entry></feed>"#;
        let items = parse(atom).unwrap();
        assert_eq!(items[0].hash, token::hash("urn:a"));
        assert_eq!(items[0].link, "https://blog.example/a");
        assert_eq!(
            items[0].image.as_deref(),
            Some("https://blog.example/a.png")
        );
        assert!(items[0].date.is_some());
        assert!(parse(b"<html><body>no feed</body></html>").is_err());
    }

    #[test]
    fn only_absolute_web_links_reach_a_post() {
        let rss = |site: &str, links: &[&str]| {
            let items: String = links
                .iter()
                .enumerate()
                .map(|(n, l)| {
                    format!("<item><guid>g{n}</guid><title>T</title><link>{l}</link></item>")
                })
                .collect();
            format!("<rss version=\"2.0\"><channel><title>B</title><link>{site}</link>{items}</channel></rss>")
        };
        let bad = [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "data:text/html,hi",
            "ftp://blog.example/a",
            "file:///etc/passwd",
            "/posts/a",
            "posts/a",
            "//blog.example/a",
            "blog.example/a",
        ];
        let mut links = bad.to_vec();
        links.extend(["https://blog.example/a", " http://blog.example/b "]);
        // A bad link falls back to the feed's site, a good one stays.
        let items = parse(rss("https://blog.example/", &links).as_bytes()).unwrap();
        assert_eq!(items.len(), links.len());
        for (item, link) in items.iter().zip(&bad) {
            assert_eq!(item.link, "https://blog.example/", "{link}");
        }
        assert_eq!(items[bad.len()].link, "https://blog.example/a");
        assert_eq!(items[bad.len() + 1].link, "http://blog.example/b");
        // With no web site link either, the item has no link at all.
        for site in ["javascript:alert(1)", "/home", ""] {
            let items = parse(rss(site, &bad).as_bytes()).unwrap();
            assert_eq!(items.len(), bad.len());
            assert!(items.iter().all(|i| i.link.is_empty()), "{site}");
            let body = render("{title} {link}", "bluesky", &items[0]);
            assert_eq!(body, "T ");
        }
        // An item is still known by the link it was written with, so each stays its own.
        let two = parse(rss("", &["/a", "/b"]).as_bytes()).unwrap();
        assert_ne!(two[0].hash, two[1].hash);
    }

    #[tokio::test]
    async fn a_parser_panic_is_a_failed_read_not_a_dead_task() {
        let broke: Result<(), String> = guarded(|| panic!("parser bug")).await;
        assert_eq!(broke, Err(UNREADABLE.to_owned()));
        assert_eq!(guarded(|| Ok(1)).await, Ok(1));
        assert!(parsed(b"<rss version=\"2.0\"><channel/></rss>".to_vec())
            .await
            .is_ok());
        let not = parsed(b"nope".to_vec()).await;
        assert_eq!(not.err().as_deref(), Some("not an RSS or Atom feed"));
    }

    #[test]
    fn only_public_https_urls_and_the_test_origin_pass() {
        let prod = Fetcher::new(None);
        assert!(prod.allows("https://blog.example/feed.xml"));
        for bad in [
            "http://blog.example/feed.xml",
            "https://127.0.0.1/feed",
            "https://10.0.0.5/feed",
            "https://169.254.169.254/latest",
            "https://[::1]/feed",
            "https://localhost/feed",
            "https://user:pw@blog.example/feed",
            "http://127.0.0.1:4010/feed",
            "file:///etc/passwd",
        ] {
            assert!(!prod.allows(bad), "{bad}");
        }
        let test = Fetcher::new(Some("http://127.0.0.1:4010".into()));
        assert!(test.allows("http://127.0.0.1:4010/feed"));
        assert!(!test.allows("http://127.0.0.1:4011/feed"));
        assert!(!test.allows("http://10.0.0.5:4010/feed"));
    }
}
