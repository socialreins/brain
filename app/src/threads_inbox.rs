//! Threads in the inbox: replies to the account's posts (kind `comment`) and posts that tag it
//! (kind `mention`), read into `inbox_items` like Bluesky's, so filters, inboxes, contacts and
//! automations work on them unchanged. Threads has no direct messages and no like call for apps.
//!
//! Reading is by polling, since Threads webhooks reach only an approved app (`webhook` turns a
//! delivery into an early poll of the accounts it names, nothing more):
//! - a fast pass every 5 minutes reads the account's mentions, the last 2 days of its posts and the
//!   replies under those that have any;
//! - an hourly pass reads the whole 7 days again (the gap recovery), and probes that
//!   `threads_read_replies` was granted (a reply to a post older than 7 days is not read);
//! - a new connection (`threads_synced_at` null) reads the 7 days once and stores everything as
//!   already automated, and so does any item older than a day when it is first read, so no
//!   automation answers old ones.
//!
//! Each account's read has a time limit (`READ_BUDGET`) and a few accounts run a minute, so
//! many accounts fit; what a read did not reach waits for the next pass. A reply is kept when it
//! is directly under one of the account's posts or one of its replies and is not the account's
//! own. An account that connected before the inbox scopes gets `meta.replies_denied` the first
//! time Threads refuses a read (the web asks it to reconnect) and is left alone until then.
//! Sends (a public reply) go through `inbox::send`, once, and a reply or a hide is never retried.
use std::{
    collections::HashSet,
    time::{Duration as Wait, Instant},
};

use chrono::{DateTime, Duration, FixedOffset, Utc};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, EntityTrait, Statement, Value as Param};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    automations,
    connectors::{
        bluesky::{Author, InboxError, PostError},
        threads::RATE_LIMITED,
        UNSENT,
    },
    inbox::{self, Attachment, Fresh},
    models::{inbox_items, social_accounts},
    state::AppState,
};

/// An account is read again this long after its last read; `READ_ACCOUNTS` start a tick.
const FAST_EVERY_MINUTES: i64 = 5;
const READ_ACCOUNTS: i64 = 6;
/// The hourly pass reads the whole window and probes the permission.
const FULL_EVERY_MINUTES: i64 = 60;
/// A fast pass reads this many days of posts; a full or first pass `inbox::FIRST_DAYS`.
const FAST_DAYS: i64 = 2;
/// Longest one account's read takes; what it did not reach waits for the next pass.
const READ_BUDGET: Wait = Wait::from_secs(25);
/// Pages one list is followed for, and the most posts whose replies one pass reads.
const PAGES: usize = 3;
const MAX_POSTS: usize = 40;
/// Items older than this when first read are stored as already automated.
const AUTOMATE_WITHIN_HOURS: i64 = 24;
/// After Threads rate limits an account's read, it is left alone this long.
const RETRY_MINUTES: i32 = 5;
/// After Threads refuses the token, this long.
const PAUSE_MINUTES: i32 = 60;
/// Posts walked up for the context above an item.
const CONTEXT_LEVELS: usize = 6;

fn sql(sql: &str, values: Vec<Param>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

fn text(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

fn id(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A time as Threads writes it (`2026-11-25T10:00:00+0000`).
fn time(v: &Value) -> Option<DateTime<FixedOffset>> {
    let s = v.as_str()?;
    DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%z")
        .or_else(|_| DateTime::parse_from_rfc3339(s))
        .ok()
}

fn https(v: &Value, key: &str) -> Option<String> {
    text(v, key).filter(|u| u.starts_with("https://"))
}

fn attachments(v: &Value) -> Vec<Attachment> {
    let mut out = Vec::new();
    let url = text(v, "media_url");
    let preview = text(v, "thumbnail_url");
    match v["media_type"].as_str() {
        Some("IMAGE") => out.extend(Attachment::new("image", url.as_deref(), None)),
        Some("VIDEO") => out.extend(Attachment::new("video", url.as_deref(), preview.as_deref())),
        Some("AUDIO") => out.extend(Attachment::new("audio", url.as_deref(), None)),
        _ => {}
    }
    out.extend(Attachment::new("gif", text(v, "gif_url").as_deref(), None));
    out
}

/// A reply or a mention as an item. A reply is a `comment`, a post that tags the account a
/// `mention`. Threads names no id for an author, only a username (and none for a private
/// profile), so the username is the contact's key; a private author is its own contact.
fn fresh(v: &Value, kind: &'static str) -> Option<Fresh> {
    let remote_id = id(&v["id"])?;
    let user = text(v, "username");
    Some(Fresh {
        kind,
        parent: id(&v["replied_to"]["id"]),
        root: (id(&v["root_post"]["id"]), None),
        author: Author {
            did: user
                .clone()
                .unwrap_or_else(|| format!("private:{remote_id}")),
            handle: user,
            display_name: None,
            avatar: https(v, "profile_picture_url"),
        },
        text: text(v, "text").unwrap_or_default(),
        at: time(&v["timestamp"])?,
        permalink: https(v, "permalink"),
        attachments: attachments(v),
        remote_id,
        remote_cid: None,
        convo_id: None,
        story: None,
        button_payload: None,
    })
}

/// The replies of one conversation page worth an item: directly under the root post or under
/// one of the account's own replies (`is_reply_owned_by_me`), and not the account's own.
fn replies_to_us(root: &str, all: &[Value]) -> Vec<Value> {
    let ours: HashSet<String> = std::iter::once(root.to_owned())
        .chain(
            all.iter()
                .filter(|r| r["is_reply_owned_by_me"] == true)
                .filter_map(|r| id(&r["id"])),
        )
        .collect();
    all.iter()
        .filter(|r| r["is_reply_owned_by_me"] != true)
        .filter(|r| id(&r["replied_to"]["id"]).is_some_and(|p| ours.contains(&p)))
        .cloned()
        .collect()
}

/// What a pass found, before it is stored.
#[derive(Default)]
struct Found {
    items: Vec<Fresh>,
}

/// Which of `ids` the account already holds.
async fn known(
    db: &impl ConnectionTrait,
    account: Uuid,
    ids: &[String],
) -> Result<HashSet<String>, DbErr> {
    let stmt = sql(
        "SELECT remote_id FROM inbox_items WHERE social_account_id = $1 \
         AND remote_id = ANY($2)",
        vec![account.into(), ids.to_vec().into()],
    );
    let rows = db.query_all_raw(stmt).await?;
    rows.iter().map(|r| r.try_get("", "remote_id")).collect()
}

/// What stops a read: the account's token is dead or a permission is missing (the account is
/// marked, and the pass ends), or a rate limit (it waits). Anything else is logged and the pass
/// goes on with what it has.
async fn failed(
    app: &AppState,
    account: &social_accounts::Model,
    section: &str,
    e: &InboxError,
) -> Result<bool, DbErr> {
    let stmt = match e {
        InboxError::ThreadsReconnect => sql(
            "UPDATE social_accounts SET inbox_error_at = now() WHERE id = $1",
            vec![account.id.into()],
        ),
        InboxError::RateLimited => sql(
            "UPDATE social_accounts SET inbox_retry_at = now() + make_interval(mins => $2) \
             WHERE id = $1",
            vec![account.id.into(), RETRY_MINUTES.into()],
        ),
        // Only the probe says the permission is missing: one post's refusal says nothing of the rest.
        InboxError::ThreadsDenied if section == "probe" => sql(
            "UPDATE social_accounts SET meta = meta || '{\"replies_denied\": true}'::jsonb \
             WHERE id = $1",
            vec![account.id.into()],
        ),
        InboxError::ThreadsDenied if section == "mentions" => sql(
            "UPDATE social_accounts SET meta = meta || '{\"mentions_denied\": true}'::jsonb \
             WHERE id = $1",
            vec![account.id.into()],
        ),
        _ => {
            tracing::warn!(code = e.code(), account = %account.id, section, "threads read failed");
            return Ok(false);
        }
    };
    app.db.execute_raw(stmt).await?;
    if *e == InboxError::ThreadsReconnect {
        crate::expiry::flag_reconnect(&app.db, account.id).await;
    }
    // A missing mentions permission leaves the replies readable.
    Ok(*e != InboxError::ThreadsDenied || section == "probe")
}

fn denied(account: &social_accounts::Model, key: &str) -> bool {
    account.meta[key].as_bool() == Some(true)
}

/// The account's replies and mentions of the last days, stored; the new items an automation may
/// still answer (younger than a day, and not the first read of the account).
/// `first`: the account was never read. `full`: read the whole window, not the last 2 days.
pub async fn read(
    app: &AppState,
    account: &social_accounts::Model,
    (first, full): (bool, bool),
) -> Result<Vec<inbox_items::Model>, DbErr> {
    let Some(token) = inbox::secret(app, account) else {
        return Ok(Vec::new());
    };
    let deadline = Instant::now() + READ_BUDGET;
    let days = if first || full {
        inbox::FIRST_DAYS
    } else {
        FAST_DAYS
    };
    let floor = Utc::now() - Duration::days(days);
    let user = account.external_id.as_str();
    let mut found = Found::default();
    let t = &app.threads;

    // A username can change and the loop guard finds our accounts by it, so it is read again on
    // the hourly pass (a failed read keeps the old one).
    if first || full {
        let named = app.threads.me(&token).await.ok().and_then(|p| p.username);
        if let Some(name) = named.filter(|n| Some(n) != account.handle.as_ref()) {
            let stmt = sql(
                "UPDATE social_accounts SET handle = $2 WHERE id = $1",
                vec![account.id.into(), name.into()],
            );
            app.db.execute_raw(stmt).await?;
        }
    }
    if (first || full) && !denied(account, "replies_denied") {
        if let Err(e) = t.probe_replies(user, &token).await {
            if failed(app, account, "probe", &e).await? {
                return Ok(Vec::new());
            }
        }
    }
    // Mentions first: they are few, and the replies of many posts must not use up their time.
    if !denied(account, "mentions_denied") {
        let mut after: Option<String> = None;
        for _ in 0..PAGES {
            if Instant::now() >= deadline {
                break;
            }
            match t
                .mentions(user, &token, floor.timestamp(), after.as_deref())
                .await
            {
                Ok(page) => {
                    for m in page["data"].as_array().into_iter().flatten() {
                        // The account's own posts that tag itself are not for the inbox.
                        if m["is_reply_owned_by_me"] != true
                            && text(m, "username") != account.handle
                        {
                            found.items.extend(fresh(m, "mention"));
                        }
                    }
                    after = text(&page["paging"]["cursors"], "after")
                        .filter(|_| page["paging"]["next"].is_string());
                }
                Err(e) => {
                    if failed(app, account, "mentions", &e).await? {
                        return Ok(Vec::new());
                    }
                    break;
                }
            }
            if after.is_none() {
                break;
            }
        }
    }
    // The posts that have replies, newest first.
    let mut posts: Vec<String> = Vec::new();
    let since = floor.format("%Y-%m-%d").to_string();
    let mut after: Option<String> = None;
    'pages: for _ in 0..PAGES {
        if Instant::now() >= deadline {
            break;
        }
        let page = match t.my_posts(user, &token, &since, after.as_deref()).await {
            Ok(p) => p,
            Err(e) => {
                if failed(app, account, "posts", &e).await? {
                    return Ok(Vec::new());
                }
                break;
            }
        };
        for p in page["data"].as_array().into_iter().flatten() {
            let young = time(&p["timestamp"]).is_some_and(|at| at >= floor);
            if let (true, true, Some(i)) = (young, p["has_replies"] == true, id(&p["id"])) {
                posts.push(i);
            }
            if posts.len() >= MAX_POSTS {
                break 'pages;
            }
        }
        after = text(&page["paging"]["cursors"], "after")
            .filter(|_| page["paging"]["next"].is_string());
        if after.is_none() {
            break;
        }
    }
    if !denied(account, "replies_denied") {
        for post in &posts {
            if Instant::now() >= deadline {
                tracing::warn!(account = %account.id, "threads read out of time");
                break;
            }
            let mut all: Vec<Value> = Vec::new();
            let mut after: Option<String> = None;
            for _ in 0..PAGES {
                if Instant::now() >= deadline {
                    tracing::warn!(account = %account.id, "threads read out of time");
                    break;
                }
                match t.conversation(&token, post, after.as_deref()).await {
                    Ok(page) => {
                        all.extend(page["data"].as_array().cloned().unwrap_or_default());
                        after = text(&page["paging"]["cursors"], "after")
                            .filter(|_| page["paging"]["next"].is_string());
                    }
                    Err(e) => {
                        if failed(app, account, "replies", &e).await? {
                            return store(app, account, found, first).await;
                        }
                        break;
                    }
                }
                if after.is_none() {
                    break;
                }
            }
            found.items.extend(
                replies_to_us(post, &all)
                    .iter()
                    .filter_map(|r| fresh(r, "comment")),
            );
        }
    }
    store(app, account, found, first).await
}

/// Stores what a pass found, oldest first, and gives back the new items an automation may
/// answer. A reply that is also a mention is a `comment` (replies are read first).
async fn store(
    app: &AppState,
    account: &social_accounts::Model,
    found: Found,
    first: bool,
) -> Result<Vec<inbox_items::Model>, DbErr> {
    let mut items = found.items;
    // Oldest first; of one post read twice, the reply (a `comment`) goes before its mention.
    items.sort_by_key(|f| (f.at, f.kind != "comment"));
    let mut seen = HashSet::new();
    items.retain(|f| seen.insert(f.remote_id.clone()));
    let ids: Vec<String> = items.iter().map(|f| f.remote_id.clone()).collect();
    let held = known(&app.db, account.id, &ids).await?;
    let old = Utc::now() - Duration::hours(AUTOMATE_WITHIN_HOURS);
    let mut fresh = Vec::new();
    for f in items.into_iter().filter(|f| !held.contains(&f.remote_id)) {
        let stale = first || f.at < old || f.at < account.created_at;
        if let Some(item) =
            inbox::store_with(&app.db, account, f, (false, stale), app.billing_enabled).await?
        {
            if !stale {
                fresh.push(item);
            }
        }
    }
    Ok(fresh)
}

/// One round: claims the accounts due (never read, or not for 5 minutes) that are not known to
/// need a reconnect or the replies permission, at most `READ_ACCOUNTS`, by stamping them so a
/// second instance skips them and a crash costs one interval, and reads each. An account whose
/// token was refused in the last hour, or that was rate limited in the last minutes, waits.
pub async fn tick(app: &AppState) -> Result<(), DbErr> {
    let claim = sql(
        "WITH due AS (SELECT id, threads_synced_at IS NULL AS first, \
           (threads_full_at IS NULL OR threads_full_at < now() - make_interval(mins => $2)) \
             AS full \
         FROM social_accounts WHERE provider = 'threads' \
         AND (threads_synced_at IS NULL OR threads_synced_at < now() - make_interval(mins => $1)) \
         AND (inbox_error_at IS NULL OR inbox_error_at < now() - make_interval(mins => $3)) \
         AND (inbox_retry_at IS NULL OR inbox_retry_at < now()) \
         AND COALESCE(meta->>'reconnect', 'false') <> 'true' \
         AND COALESCE(meta->>'replies_denied', 'false') <> 'true' \
         ORDER BY threads_synced_at NULLS FIRST LIMIT $4 FOR UPDATE SKIP LOCKED) \
         UPDATE social_accounts a SET threads_synced_at = now(), \
           threads_full_at = CASE WHEN due.full THEN now() ELSE a.threads_full_at END \
         FROM due WHERE a.id = due.id RETURNING a.id, due.first, due.full",
        vec![
            (FAST_EVERY_MINUTES as i32).into(),
            (FULL_EVERY_MINUTES as i32).into(),
            PAUSE_MINUTES.into(),
            READ_ACCOUNTS.into(),
        ],
    );
    for row in app.db.query_all_raw(claim).await? {
        let id: Uuid = row.try_get("", "id")?;
        let (first, full): (bool, bool) = (row.try_get("", "first")?, row.try_get("", "full")?);
        let Some(account) = social_accounts::Entity::find_by_id(id).one(&app.db).await? else {
            continue;
        };
        match read(app, &account, (first, full)).await {
            // A reply goes out after about 30 seconds, so the sends do not hold up the next account.
            Ok(items) => {
                let app = app.clone();
                tokio::spawn(async move {
                    for item in items {
                        if let Err(e) = automations::run(&app, &account, &item).await {
                            tracing::error!(error = %e, item = %item.id, "automations failed");
                        }
                    }
                });
            }
            Err(e) => tracing::error!(error = %e, account = %id, "threads read failed"),
        }
    }
    Ok(())
}

/// A delivery of the Threads webhook names these users (a mention's `target_id`, the owner of
/// the post a reply is under): their accounts are read at the next tick instead of waiting for
/// their turn. Throttled to one early read in 30 seconds an account.
pub async fn hint(app: &AppState, users: &[String]) -> Result<u64, DbErr> {
    if users.is_empty() {
        return Ok(0);
    }
    let stmt = sql(
        "UPDATE social_accounts SET threads_synced_at = now() - make_interval(mins => $2) \
         WHERE provider = 'threads' AND external_id = ANY($1) AND threads_synced_at IS NOT NULL \
         AND threads_synced_at < now() - interval '30 seconds'",
        vec![users.to_vec().into(), (FAST_EVERY_MINUTES as i32).into()],
    );
    app.db.execute_raw(stmt).await.map(|r| r.rows_affected())
}

/// The users a webhook delivery names. The envelope is read loosely (the docs show `target_id`
/// and `values[].value`, Meta's others `entry[].changes[].value`): anything unrecognised names
/// nobody, and a delivery only ever causes a read, never a stored item.
pub fn users_in(body: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let mut take = |v: &Value| out.extend(id(v));
    let mut walk = |notice: &Value| {
        take(&notice["target_id"]);
        for v in notice["values"].as_array().into_iter().flatten() {
            take(&v["value"]["root_post"]["owner_id"]);
        }
    };
    walk(body);
    for e in body["entry"].as_array().into_iter().flatten() {
        take(&e["id"]);
        for c in e["changes"].as_array().into_iter().flatten() {
            take(&c["value"]["root_post"]["owner_id"]);
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A public reply under `item` as `account`, once; the id of the reply.
pub async fn send(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    text: &str,
) -> Result<String, InboxError> {
    if !matches!(item.kind.as_str(), "comment" | "mention") {
        return Err(InboxError::Invalid("not_repliable"));
    }
    let Some(token) = inbox::secret(app, account) else {
        return Err(InboxError::ThreadsReconnect);
    };
    // A post that tagged us may limit who can reply to it: one for its own author only takes no
    // reply from us, which is told before anything is sent. The other limits (followers, accounts
    // the author follows) cannot be judged from here, so Threads has the last word on them.
    if item.kind == "mention"
        && app
            .threads
            .reply_audience(&token, &item.remote_id)
            .await
            .as_deref()
            == Some("PARENT_POST_AUTHOR_ONLY")
    {
        return Err(InboxError::Invalid("threads_replies_closed"));
    }
    // A reply is a post made with `reply_to_id`, so it waits like a post does.
    let made = app
        .threads
        .publish_post(
            &account.external_id,
            &token,
            text,
            &[],
            Some(&item.remote_id),
        )
        .await;
    made.map(|(id, _)| id).map_err(|e| match e {
        PostError::Reconnect => InboxError::ThreadsReconnect,
        PostError::Denied => InboxError::ThreadsDenied,
        PostError::Failed(why) if why.contains(RATE_LIMITED) => InboxError::RateLimited,
        // Without the "Nothing was posted." mark the reply may be up: say so, so nobody sends it twice.
        PostError::Failed(why) if !why.contains(UNSENT) => InboxError::Threads(format!(
            "{why} It may have been posted: check Threads before trying again."
        )),
        PostError::Failed(why) => InboxError::Threads(why),
    })
}

/// Hides or shows again the reply of `item`, which Threads allows for a reply directly under a
/// post of the account (`manage_reply`). Every other action is refused before any call: apps
/// cannot like, repost, follow, mute or block on Threads. The id of nothing: `Ok(None)`.
pub async fn perform(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    hide: Option<bool>,
) -> Result<Option<String>, InboxError> {
    let Some(hide) = hide else {
        return Err(InboxError::Invalid("threads_no_action"));
    };
    if item.kind != "comment" || Some(&item.author_id) == account.handle.as_ref() {
        return Err(InboxError::Invalid("not_our_thread"));
    }
    // Only a reply directly under the post: its parent is the thread's root.
    if item.parent_remote_id.is_none() || item.parent_remote_id != item.root_remote_id {
        return Err(InboxError::Invalid("threads_nested"));
    }
    let Some(token) = inbox::secret(app, account) else {
        return Err(InboxError::ThreadsReconnect);
    };
    let done = app
        .threads
        .manage_reply(&token, &item.remote_id, hide)
        .await;
    if done == Err(InboxError::ThreadsReconnect) {
        crate::expiry::flag_reconnect(&app.db, account.id).await;
    }
    done.map(|()| None)
}

/// One post above an item.
pub struct Above {
    pub id: String,
    pub author: Author,
    pub text: String,
    pub at: DateTime<FixedOffset>,
    pub permalink: Option<String>,
    pub attachments: Vec<Attachment>,
}

/// The posts above `item`, oldest first: the post it answers, then that post's parent, up to
/// `CONTEXT_LEVELS`; `true` when the chain was cut. A mention that is no reply has none.
pub async fn above(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
) -> Result<(Vec<Above>, bool), InboxError> {
    let Some(token) = inbox::secret(app, account) else {
        return Err(InboxError::ThreadsReconnect);
    };
    let mut chain = Vec::new();
    let mut next = item.parent_remote_id.clone();
    let mut cut = false;
    while let Some(post) = next.take() {
        if chain.len() >= CONTEXT_LEVELS {
            cut = true;
            break;
        }
        let v = match app.threads.post(&token, &post).await {
            Ok(v) => v,
            // The rest of the chain stays out; what was read is shown.
            Err(InboxError::Threads(_)) if !chain.is_empty() => {
                cut = true;
                break;
            }
            Err(InboxError::Threads(_)) => return Err(InboxError::PostGone),
            Err(e) => return Err(e),
        };
        let Some(f) = fresh(&v, "comment") else { break };
        next = f.parent.clone().filter(|_| v["is_reply"] == true);
        // Our own post: the account's name, not a missing one.
        let own = v["is_reply_owned_by_me"] == true || f.author.handle == account.handle;
        let mut author = f.author;
        if own {
            author.did = account.external_id.clone();
            author.handle = account.handle.clone();
        }
        chain.push(Above {
            id: f.remote_id,
            author,
            text: f.text,
            at: f.at,
            permalink: f.permalink,
            attachments: f.attachments,
        });
    }
    chain.reverse();
    Ok((chain, cut))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reply(id: &str, to: &str, mine: bool) -> Value {
        json!({"id": id, "text": "t", "username": if mine { "us" } else { "ann" },
            "timestamp": "2026-11-25T10:00:00+0000", "is_reply": true,
            "is_reply_owned_by_me": mine, "replied_to": {"id": to}, "root_post": {"id": "100"}})
    }

    #[test]
    fn only_replies_to_us_are_kept() {
        let all = vec![
            reply("1", "100", false),
            reply("2", "100", true),
            reply("3", "2", false),
            reply("4", "1", false),
        ];
        let kept: Vec<String> = replies_to_us("100", &all)
            .iter()
            .filter_map(|r| id(&r["id"]))
            .collect();
        // 1 is under our post, 3 under our reply 2; 2 is ours and 4 answers someone else.
        assert_eq!(kept, ["1", "3"]);
    }

    #[test]
    fn a_reply_becomes_an_item_with_its_context() {
        let mut v = reply("7", "100", false);
        v["media_type"] = "IMAGE".into();
        v["media_url"] = "https://cdn.example/a.jpg".into();
        v["permalink"] = "https://www.threads.com/@ann/post/abc".into();
        let f = fresh(&v, "comment").unwrap();
        assert_eq!(f.kind, "comment");
        assert_eq!(f.remote_id, "7");
        assert_eq!(f.parent.as_deref(), Some("100"));
        assert_eq!(f.root.0.as_deref(), Some("100"));
        assert_eq!(f.author.did, "ann");
        assert_eq!(f.attachments.len(), 1);
        assert_eq!(f.at.timestamp(), 1_795_600_800);
        // No username (a private profile): its own contact, never a shared one.
        let mut anon = reply("8", "100", false);
        anon["username"] = Value::Null;
        assert_eq!(fresh(&anon, "comment").unwrap().author.did, "private:8");
        // No time, no item.
        let mut none = reply("9", "100", false);
        none["timestamp"] = Value::Null;
        assert!(fresh(&none, "comment").is_none());
    }

    #[test]
    fn a_webhook_names_the_users_it_is_about() {
        let reply = json!({"target_id": "555", "values": [{"field": "replies",
            "value": {"id": "9", "root_post": {"id": "1", "owner_id": "42"}}}]});
        assert_eq!(users_in(&reply), ["42", "555"]);
        let mention =
            json!({"target_id": "42", "values": [{"field": "mentions", "value": {"id": "3"}}]});
        assert_eq!(users_in(&mention), ["42"]);
        let meta = json!({"entry": [{"id": "42", "changes": [{"value":
            {"root_post": {"owner_id": "43"}}}]}]});
        assert_eq!(users_in(&meta), ["42", "43"]);
        assert!(users_in(&json!({"hello": 1})).is_empty());
    }
}
