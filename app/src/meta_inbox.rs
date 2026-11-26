//! Facebook and Instagram in the inbox. `POST /webhooks/meta` (`controllers/meta_webhook.rs`)
//! hands each signed delivery to `ingest`, which stores comments (Page `feed`, Instagram
//! `comments`) and direct messages (`messaging`) of every connected account, across all teams,
//! into `inbox_items` with the same unique key as Bluesky, so a repeated delivery adds nothing.
//! `follow_up` then fills in what the webhook leaves out (a sender's name, an Instagram link)
//! and runs automations on new items. An Instagram reply to a story (`message.reply_to.story`)
//! is a `dm` with `story_reply` set: it runs `story_reply` automations, not `dm_received` ones.
//! A click on a quick reply button (`message.quick_reply.payload`) is a `dm` with
//! `button_payload` set: it runs `button_clicked` automations, not `dm_received` ones either.
//! Replies and DMs go out through `send`, once, never retried.
//!
//! The webhook is the only live source, so a delivery Meta never made (an outage of ours, or an
//! account connected a week after its comments) would stay missing. `tick` therefore reads each
//! account's last 7 days again, on connect and then hourly (`backfill`): the Page's recent
//! posts and their comments, or the Instagram account's recent media and their comments, and the
//! Page's conversations. Items go in with the webhook's unique key, so nothing is stored twice.
//! A backfilled item younger than a day is left for the automation sweep (its comment-to-DM
//! still works, within Meta's windows); an older one is marked automated. The first read of an
//! account marks every item automated, so connecting an account (or deploying this) never
//! answers a week of old comments.
use std::{
    collections::HashSet,
    sync::LazyLock,
    time::{Duration as Wait, Instant},
};

use chrono::{DateTime, Duration, FixedOffset, Utc};
use sea_orm::ColumnTrait;
use sea_orm::{ConnectionTrait, DbBackend, DbErr, EntityTrait, QueryFilter, Statement};
use serde_json::Value;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::{
    automations,
    connectors::{
        bluesky::{Author, InboxError},
        meta::{Moderate, To},
    },
    inbox::{self, Attachment, Button, Fresh, Via},
    models::{inbox_items, social_accounts},
    state::AppState,
};

/// A private reply to a comment works within 7 days of it.
pub(crate) const PRIVATE_REPLY_DAYS: i64 = 7;
/// A message to a person works within 24 hours of their last message.
pub(crate) const MESSAGE_HOURS: i64 = 24;
/// Longest quick reply payload kept; ours are at most 64.
const MAX_PAYLOAD_CHARS: usize = 200;
/// A person's reply, with Meta's Human Agent feature, may go out this long after their last message.
pub(crate) const HUMAN_AGENT_DAYS: i64 = 7;
/// An account is read again this long after its last read. A tick claims `BACKFILL_CHUNK`
/// accounts at a time, oldest read first, reads `BACKFILL_SLOTS` at once and claims the next chunk
/// until nothing is due or `TICK_BUDGET` is spent, so a thousand accounts fit in the hour (it was
/// 4 a minute, one after another: 240 an hour at best).
const BACKFILL_EVERY_MINUTES: i64 = 60;
const BACKFILL_CHUNK: i64 = 12;
const BACKFILL_SLOTS: usize = 4;
/// No new chunk is claimed after this; the next tick carries on.
const TICK_BUDGET: Wait = Wait::from_secs(45);
/// Backfills running at once across the process (about 110 Graph calls each, one Page each).
static SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(BACKFILL_SLOTS));
/// Longest one account's read takes; what it reached is kept and the rest waits for the next
/// hour (shortcut: no resume cursor, since a read is capped at about 110 calls).
const BACKFILL_BUDGET: Wait = Wait::from_secs(30);
/// Items asked for in a call, and the most pages one list is followed for.
const PAGE_SIZE: &str = "25";
const POST_PAGES: usize = 2;
const COMMENT_PAGES: usize = 2;
const CONVERSATION_PAGES: usize = 4;
/// Messages read of each conversation (the newest).
const CONVERSATION_MESSAGES: &str = "10";
/// A backfilled item older than this is stored but never answered by an automation.
const AUTOMATE_WITHIN_HOURS: i64 = 24;

fn sid(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn at(secs: Option<i64>, millis: Option<i64>) -> DateTime<FixedOffset> {
    let from_ms = millis.and_then(DateTime::from_timestamp_millis);
    from_ms
        .or_else(|| secs.and_then(|s| DateTime::from_timestamp(s, 0)))
        .unwrap_or_else(Utc::now)
        .fixed_offset()
}

/// What a Facebook comment carries besides text: a `photo` or `video` link, or an `attachment`
/// (a sticker or GIF comes as one, with `media.image.src`).
fn comment_attachments(v: &Value) -> Vec<Attachment> {
    let a = &v["attachment"];
    let attached = a.is_object().then(|| {
        let kind = match a["type"].as_str().unwrap_or_default() {
            "sticker" => "sticker",
            t if t.starts_with("animated_image") => "gif",
            t if t.starts_with("video") => "video",
            "photo" => "image",
            _ => "link",
        };
        let src = a["media"]["image"]["src"].as_str();
        match kind {
            "link" => Attachment::new(kind, a["url"].as_str(), src),
            _ => Attachment::new(kind, src.or(a["url"].as_str()), src),
        }
    });
    [
        Attachment::new("image", v["photo"].as_str(), None),
        Attachment::new("video", v["video"].as_str(), None),
    ]
    .into_iter()
    .chain(attached)
    .flatten()
    .collect()
}

/// A message's `attachments` (Messenger and Instagram): Messenger sends a sticker as an `image`
/// with a `sticker_id`.
fn message_attachments(msg: &Value) -> Vec<Attachment> {
    let files = msg["attachments"].as_array().into_iter().flatten();
    files
        .filter_map(|f| {
            let p = &f["payload"];
            let kind = match f["type"].as_str()? {
                "image" if !p["sticker_id"].is_null() => "sticker",
                "image" => "image",
                "animated_image" => "gif",
                "video" | "ig_reel" | "reel" => "video",
                "audio" => "audio",
                "file" => "file",
                "story_mention" => "story_mention",
                "share" | "fallback" | "template" => "link",
                _ => return None,
            };
            Attachment::new(kind, p["url"].as_str(), None)
        })
        .take(inbox::MAX_ATTACHMENTS)
        .collect()
}

/// A new Facebook comment (`item=comment`, `verb=add`) on a Page post, or a reply to one.
fn facebook_comment(v: &Value, entry_time: Option<i64>) -> Option<Fresh> {
    if v["item"] != "comment" || v["verb"] != "add" {
        return None;
    }
    let (id, post) = (sid(&v["comment_id"])?, sid(&v["post_id"])?);
    let from = &v["from"];
    let parent = sid(&v["parent_id"]).unwrap_or_else(|| post.clone());
    Some(Fresh {
        kind: "comment",
        remote_id: id,
        remote_cid: None,
        parent: Some(parent),
        root: (Some(post.clone()), None),
        convo_id: None,
        story: None,
        button_payload: None,
        author: Author {
            did: sid(&from["id"])?,
            display_name: sid(&from["name"]),
            ..Author::default()
        },
        text: v["message"].as_str().unwrap_or_default().to_owned(),
        at: at(v["created_time"].as_i64().or(entry_time), None),
        permalink: Some(format!("https://www.facebook.com/{post}")),
        attachments: comment_attachments(v),
    })
}

/// A new Instagram comment, or a reply to one. Its link is looked up later (`follow_up`).
fn instagram_comment(v: &Value, entry_time: Option<i64>) -> Option<Fresh> {
    let (id, media) = (sid(&v["id"])?, sid(&v["media"]["id"])?);
    let from = &v["from"];
    let parent = sid(&v["parent_id"]).unwrap_or_else(|| media.clone());
    Some(Fresh {
        kind: "comment",
        remote_id: id,
        remote_cid: None,
        parent: Some(parent),
        root: (Some(media), None),
        convo_id: None,
        story: None,
        button_payload: None,
        author: Author {
            did: sid(&from["id"])?,
            handle: sid(&from["username"]),
            ..Author::default()
        },
        text: v["text"].as_str().unwrap_or_default().to_owned(),
        at: at(entry_time, None),
        permalink: None,
        attachments: Vec::new(),
    })
}

/// A message someone sent the Page or the Instagram account. Echoes (what the account itself
/// sent), deletions, and events that are not messages (reads, deliveries, reactions) give `None`.
fn message(m: &Value, entry_time: Option<i64>) -> Option<Fresh> {
    let msg = &m["message"];
    if !msg.is_object() || msg["is_echo"] == true || msg["is_deleted"] == true {
        return None;
    }
    let (mid, sender) = (sid(&msg["mid"])?, sid(&m["sender"]["id"])?);
    let attachments = message_attachments(msg);
    // A file we cannot show (no https link, an unknown type) still says something came.
    let text = msg["text"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            let files = msg["attachments"].as_array()?;
            let shown = !attachments.is_empty();
            (!files.is_empty()).then(|| if shown { "" } else { "[attachment]" }.to_owned())
        })?;
    let story = &msg["reply_to"]["story"];
    let story = story
        .is_object()
        .then(|| (sid(&story["id"]), sid(&story["url"])));
    // A click on a quick reply: the payload the button was sent with, cut to what we store.
    let button_payload = msg["quick_reply"]["payload"]
        .as_str()
        .filter(|p| !p.is_empty())
        .map(|p| p.chars().take(MAX_PAYLOAD_CHARS).collect());
    Some(Fresh {
        story,
        button_payload,
        kind: "dm",
        remote_id: mid,
        remote_cid: None,
        parent: None,
        root: (None, None),
        convo_id: Some(sender.clone()),
        author: Author {
            did: sender,
            ..Author::default()
        },
        text,
        at: at(entry_time, m["timestamp"].as_i64()),
        permalink: None,
        attachments,
    })
}

/// What one webhook `entry` says, as inbox items, oldest first as delivered. Anything the
/// account itself wrote (its id is the entry's) is left out.
pub fn events(object: &str, entry: &Value) -> Vec<Fresh> {
    let time = entry["time"].as_i64();
    let mut out = Vec::new();
    for change in entry["changes"].as_array().into_iter().flatten() {
        let v = &change["value"];
        out.extend(match (object, change["field"].as_str()) {
            ("page", Some("feed")) => facebook_comment(v, time),
            ("instagram", Some("comments")) => instagram_comment(v, time),
            // The dashboard's test event comes this way; live ones come as `messaging`.
            ("instagram", Some("messages")) => message(v, time),
            _ => None,
        });
    }
    for m in entry["messaging"].as_array().into_iter().flatten() {
        out.extend(message(m, time));
    }
    let me = sid(&entry["id"]).unwrap_or_default();
    out.retain(|f| f.author.did != me);
    out
}

/// What `ingest` stored, for `follow_up`.
pub type Stored = Vec<(social_accounts::Model, inbox_items::Model)>;

/// Stores what a signed delivery holds for every connected account it names, in any team, and
/// pushes the accounts and new items to `fresh` for `follow_up`. Unknown accounts, items already
/// stored and the account's own comments are left out. On an error `fresh` still holds what was
/// stored before it: a redelivery finds those items already there, so their follow-up (and
/// automations) must run now.
pub async fn ingest(app: &AppState, payload: &Value, fresh: &mut Stored) -> Result<(), DbErr> {
    let object = payload["object"].as_str().unwrap_or_default();
    let provider = match object {
        "page" => "facebook",
        "instagram" => "instagram",
        _ => return Ok(()),
    };
    for entry in payload["entry"].as_array().into_iter().flatten() {
        let Some(id) = sid(&entry["id"]) else {
            continue;
        };
        let found = events(object, entry);
        if found.is_empty() {
            continue;
        }
        let accounts = social_accounts::Entity::find()
            .filter(social_accounts::Column::Provider.eq(provider))
            .filter(social_accounts::Column::ExternalId.eq(id))
            .all(&app.db)
            .await?;
        for account in accounts {
            for event in &found {
                if is_ours(app, &account, event).await? {
                    continue;
                }
                if let Some(item) =
                    inbox::store(&app.db, &account, event.clone(), app.billing_enabled).await?
                {
                    fresh.push((account.clone(), item));
                }
            }
        }
    }
    Ok(())
}

/// Whether a comment is one the account itself wrote. Instagram may name the commenter by an
/// id other than the entry's, so the username is compared with the account's handle too, and
/// the ids of replies we sent are remembered.
async fn is_ours(
    app: &AppState,
    account: &social_accounts::Model,
    event: &Fresh,
) -> Result<bool, DbErr> {
    if event.kind != "comment" {
        return Ok(false);
    }
    let named = account
        .handle
        .as_deref()
        .zip(event.author.handle.as_deref());
    if named.is_some_and(|(mine, theirs)| mine.eq_ignore_ascii_case(theirs)) {
        return Ok(true);
    }
    let stmt = sql(
        "SELECT EXISTS (SELECT 1 FROM inbox_replies r JOIN inbox_items i ON i.id = r.inbox_item_id \
         WHERE i.social_account_id = $1 AND r.remote_id = $2) AS ok",
        vec![account.id.into(), event.remote_id.clone().into()],
    );
    match app.db.query_one_raw(stmt).await? {
        Some(r) => r.try_get("", "ok"),
        None => Ok(false),
    }
}

fn sql(sql: &str, values: Vec<sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// After the webhook answered: fills in each new item, then runs automations on it.
/// Failures are logged; nothing here sends twice.
pub async fn follow_up(app: &AppState, fresh: Vec<(social_accounts::Model, inbox_items::Model)>) {
    for (account, item) in fresh {
        if let Err(e) = enrich_and_run(app, &account, item).await {
            tracing::error!(error = %e, account = %account.id, "meta inbox follow-up failed");
        }
    }
}

pub(crate) async fn enrich_and_run(
    app: &AppState,
    account: &social_accounts::Model,
    mut item: inbox_items::Model,
) -> Result<(), DbErr> {
    let instagram = account.provider == "instagram";
    if let Some(token) = inbox::secret(app, account) {
        if item.kind == "dm" {
            if let Some(p) = app.meta.profile(instagram, &token, &item.author_id).await {
                // The contact learns the name with the item.
                let stmt = sql(
                    "WITH c AS (UPDATE contacts SET display_name = COALESCE($2, display_name), \
                     handle = COALESCE($3, handle), avatar_url = COALESCE($4, avatar_url) \
                     WHERE id = (SELECT contact_id FROM inbox_items WHERE id = $1)) \
                     UPDATE inbox_items SET author_name = COALESCE($2, author_name), \
                     author_handle = COALESCE($3, author_handle), \
                     author_avatar = COALESCE($4, author_avatar) WHERE id = $1 RETURNING *",
                    vec![
                        item.id.into(),
                        p.name.into(),
                        p.handle.into(),
                        p.avatar.into(),
                    ],
                );
                // The name is what a welcome DM's `{name}` stands for.
                let named = inbox_items::Entity::find().from_raw_sql(stmt);
                if let Some(row) = named.one(&app.db).await? {
                    item = row;
                }
            }
        } else if let (true, None, Some(media)) = (instagram, &item.permalink, &item.root_remote_id)
        {
            if let Some(link) = app.meta.permalink(&token, media).await {
                let stmt = sql(
                    "UPDATE inbox_items SET permalink = $2 WHERE id = $1",
                    vec![item.id.into(), link.into()],
                );
                app.db.execute_raw(stmt).await?;
            }
        } else if let (false, "comment", None, Some(post)) = (
            instagram,
            item.kind.as_str(),
            item.post_id,
            &item.root_remote_id,
        ) {
            // A Reel's target keeps the video id, not the post id the comment names; only an
            // account with such a target (no `_`) pays for the lookup.
            if has_video_target(app, account.id).await? {
                let ids = app.meta.attached(&token, post).await;
                if !ids.is_empty() {
                    let stmt = sql(
                        "UPDATE inbox_items i SET post_id = t.post_id FROM (SELECT post_id \
                         FROM post_targets WHERE social_account_id = $2 \
                         AND remote_id IN (SELECT jsonb_array_elements_text($3)) LIMIT 1) t \
                         WHERE i.id = $1 RETURNING i.post_id",
                        vec![item.id.into(), account.id.into(), Value::from(ids).into()],
                    );
                    if let Some(r) = app.db.query_one_raw(stmt).await? {
                        item.post_id = r.try_get("", "post_id")?;
                    }
                }
            }
        }
    }
    automations::run(app, account, &item).await?;
    Ok(())
}

/// Whether the account has a target kept by a bare id (a Facebook Reel's video id).
async fn has_video_target(app: &AppState, account: Uuid) -> Result<bool, DbErr> {
    let stmt = sql(
        "SELECT EXISTS (SELECT 1 FROM post_targets WHERE social_account_id = $1 \
         AND strpos(remote_id, '_') = 0) AS ok",
        vec![account.into()],
    );
    match app.db.query_one_raw(stmt).await? {
        Some(r) => r.try_get("", "ok"),
        None => Ok(false),
    }
}

/// When the person last wrote to the account: the start of the messaging window.
async fn last_message(
    app: &AppState,
    account: Uuid,
    person: &str,
) -> Result<Option<DateTime<FixedOffset>>, DbErr> {
    let stmt = sql(
        "SELECT max(received_at) AS at FROM inbox_items WHERE social_account_id = $1 \
         AND kind = 'dm' AND author_id = $2",
        vec![account.into(), person.into()],
    );
    match app.db.query_one_raw(stmt).await? {
        Some(r) => r.try_get("", "at"),
        None => Ok(None),
    }
}

/// How long a conversation stays open for a reply, from the person's last message `last`: Meta
/// takes a message for 24 hours, and a person's reply for 7 days with the HUMAN_AGENT tag when
/// the app holds that feature (`human_agent`, else `None`).
pub struct Window {
    pub closes_at: DateTime<FixedOffset>,
    pub human_agent_until: Option<DateTime<FixedOffset>>,
}

pub fn window(last: DateTime<FixedOffset>, human_agent: bool) -> Window {
    Window {
        closes_at: last + Duration::hours(MESSAGE_HOURS),
        human_agent_until: human_agent.then(|| last + Duration::days(HUMAN_AGENT_DAYS)),
    }
}

/// What a person may do to a comment from the inbox, as the action names: hide or show,
/// delete, and (Facebook only: Instagram has no such call) like or unlike. Nothing for a comment
/// already deleted or an item that is not a comment of ours.
pub fn moderations(item: &inbox_items::Model) -> Vec<&'static str> {
    let flag = |k: &str| item.moderation[k].as_bool() == Some(true);
    let meta = matches!(item.platform.as_str(), "facebook" | "instagram");
    if !meta || item.kind != "comment" || flag("deleted") {
        return Vec::new();
    }
    let mut can = vec![if flag("hidden") { "unhide" } else { "hide" }, "delete"];
    if item.platform == "facebook" {
        can.push(if flag("liked") { "unlike" } else { "like" });
    }
    can
}

/// Moderates a comment on Meta and keeps what was done on the item (a deleted comment is also
/// `done`). The inner `Err` says why nothing was done.
pub async fn moderate(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    action: Moderate,
) -> Result<Result<(), InboxError>, DbErr> {
    let wanted = match action {
        Moderate::Hide | Moderate::Unhide => ("hidden", action == Moderate::Hide),
        Moderate::Delete => ("deleted", true),
        Moderate::Like | Moderate::Unlike => ("liked", action == Moderate::Like),
    };
    // Either way round: hiding what is hidden is allowed and does nothing new.
    let can = moderations(item);
    if !can.contains(&action.as_str()) && !can.contains(&action.flipped().as_str()) {
        return Ok(Err(InboxError::Invalid("not_supported")));
    }
    let Some(token) = inbox::secret(app, account) else {
        return Ok(Err(InboxError::MetaReconnect));
    };
    let instagram = account.provider == "instagram";
    let done = app
        .meta
        .moderate(instagram, &token, &item.remote_id, action)
        .await;
    if let Err(e) = &done {
        flag_reconnect_if_dead(app, account, e).await;
        return Ok(done);
    }
    let state = if action == Moderate::Delete {
        ", state = 'done'"
    } else {
        ""
    };
    let stmt = sql(
        &format!(
            "UPDATE inbox_items SET moderation = moderation || $2::jsonb{state} WHERE id = $1"
        ),
        vec![
            item.id.into(),
            serde_json::json!({ wanted.0: wanted.1 }).into(),
        ],
    );
    app.db.execute_raw(stmt).await?;
    Ok(Ok(()))
}

/// A token Meta refused: the account needs connecting again, which Home and Accounts then say.
pub async fn flag_reconnect_if_dead(
    app: &AppState,
    account: &social_accounts::Model,
    e: &InboxError,
) {
    if matches!(e, InboxError::MetaReconnect) {
        crate::expiry::flag_reconnect(&app.db, account.id).await;
    }
}

/// Whether a private reply to the comment already went out (Meta takes one per comment).
async fn private_reply_sent(app: &AppState, item: Uuid) -> Result<bool, DbErr> {
    let stmt = sql(
        "SELECT EXISTS (SELECT 1 FROM inbox_replies WHERE inbox_item_id = $1 AND kind = 'dm') \
         AS ok",
        vec![item.into()],
    );
    match app.db.query_one_raw(stmt).await? {
        Some(r) => r.try_get("", "ok"),
        None => Ok(false),
    }
}

/// What a read found: the items, and the ids of those someone from the account already answered
/// outside the inbox (a human replying in Messenger or on the post), which no automation may
/// answer again.
#[derive(Default)]
struct Found {
    items: Vec<Fresh>,
    answered: HashSet<String>,
}

/// A time as Graph writes it (`2026-11-25T10:00:00+0000`).
fn graph_time(v: &Value) -> Option<DateTime<FixedOffset>> {
    let s = v.as_str()?;
    DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%z")
        .or_else(|_| DateTime::parse_from_rfc3339(s))
        .ok()
}

/// A comment as Graph lists it under a Page post (`{post}/comments`), as the webhook's would
/// have been. `None` for the Page's own comments and ones that name no author.
fn listed_facebook_comment(v: &Value, post: &str, page: &str) -> Option<Fresh> {
    let (id, from) = (sid(&v["id"])?, &v["from"]);
    let author = sid(&from["id"]).filter(|a| a != page)?;
    Some(Fresh {
        kind: "comment",
        remote_id: id,
        remote_cid: None,
        parent: Some(sid(&v["parent"]["id"]).unwrap_or_else(|| post.to_owned())),
        root: (Some(post.to_owned()), None),
        convo_id: None,
        story: None,
        button_payload: None,
        author: Author {
            did: author,
            display_name: sid(&from["name"]),
            ..Author::default()
        },
        text: v["message"].as_str().unwrap_or_default().to_owned(),
        at: graph_time(&v["created_time"])?,
        permalink: Some(format!("https://www.facebook.com/{post}")),
        attachments: comment_attachments(v),
    })
}

/// An Instagram comment as Graph lists it under a media (`{media}/comments`, or inside a
/// comment's `replies`); `parent` is the media for a top-level comment, else that comment.
fn listed_instagram_comment(v: &Value, media: &str, parent: &str) -> Option<Fresh> {
    let (id, from) = (sid(&v["id"])?, &v["from"]);
    // No id, no item: a username is not the id the webhook and DMs use, and a made-up identity
    // would split one person into two contacts (verify live that Graph returns `from`).
    let author = sid(&from["id"])?;
    Some(Fresh {
        kind: "comment",
        remote_id: id,
        remote_cid: None,
        parent: Some(parent.to_owned()),
        root: (Some(media.to_owned()), None),
        convo_id: None,
        story: None,
        button_payload: None,
        author: Author {
            did: author,
            handle: sid(&from["username"]).or_else(|| sid(&v["username"])),
            ..Author::default()
        },
        text: v["text"].as_str().unwrap_or_default().to_owned(),
        at: graph_time(&v["timestamp"])?,
        permalink: None,
        attachments: Vec::new(),
    })
}

/// The messages of one listed conversation that people sent `me`, newer than `floor`.
/// Messages with no text are left out (shortcut: their attachments are not read, a webhook
/// delivery carries them).
fn listed_messages(conversation: &Value, me: &str, floor: DateTime<FixedOffset>) -> Vec<Fresh> {
    let messages = conversation["messages"]["data"].as_array();
    messages
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let (id, from) = (sid(&m["id"])?, &m["from"]);
            let sender = sid(&from["id"]).filter(|s| s != me)?;
            let text = m["message"].as_str().filter(|t| !t.is_empty())?;
            let at = graph_time(&m["created_time"]).filter(|t| *t >= floor)?;
            Some(Fresh {
                kind: "dm",
                remote_id: id,
                remote_cid: None,
                parent: None,
                root: (None, None),
                convo_id: Some(sender.clone()),
                story: None,
                button_payload: None,
                author: Author {
                    did: sender,
                    display_name: sid(&from["name"]),
                    handle: sid(&from["username"]),
                    ..Author::default()
                },
                text: text.to_owned(),
                at,
                permalink: None,
                attachments: Vec::new(),
            })
        })
        .collect()
}

/// When the account last wrote in a listed conversation.
fn last_from(conversation: &Value, me: &str) -> Option<DateTime<FixedOffset>> {
    let messages = conversation["messages"]["data"].as_array()?;
    let mine = messages
        .iter()
        .filter(|m| sid(&m["from"]["id"]).as_deref() == Some(me));
    mine.filter_map(|m| graph_time(&m["created_time"])).max()
}

/// Follows a Graph list through at most `pages` pages, giving `each` every entry until it says
/// stop (`false`), the time is up or the list ends.
async fn read_pages(
    app: &AppState,
    token: &str,
    (path, query): (&str, &[(&str, &str)]),
    (pages, deadline): (usize, Instant),
    mut each: impl FnMut(&Value) -> bool,
) -> Result<(), InboxError> {
    let mut after = String::new();
    for _ in 0..pages {
        if Instant::now() >= deadline {
            tracing::warn!(path, "meta backfill out of time");
            break;
        }
        let mut q = query.to_vec();
        if !after.is_empty() {
            q.push(("after", &after));
        }
        let body = app.meta.history(token, path, &q).await?;
        for entry in body["data"].as_array().into_iter().flatten() {
            if !each(entry) {
                return Ok(());
            }
        }
        match (
            body["paging"]["next"].is_string(),
            sid(&body["paging"]["cursors"]["after"]),
        ) {
            (true, Some(next)) => after = next,
            _ => break,
        }
    }
    Ok(())
}

/// The comments of the last days on a Facebook Page's posts (`pages_read_engagement` for the
/// list of posts, `pages_read_user_content` for their comments).
async fn facebook_comments(
    app: &AppState,
    (token, page): (&str, &str),
    (floor, deadline): (DateTime<FixedOffset>, Instant),
    out: &mut Found,
) -> Result<(), InboxError> {
    let mut posts = Vec::new();
    let feed = format!("{page}/feed");
    let query = [("fields", "id,created_time"), ("limit", PAGE_SIZE)];
    read_pages(app, token, (&feed, &query), (POST_PAGES, deadline), |p| {
        // Newest first: the first post older than the window ends the list.
        if graph_time(&p["created_time"]).is_some_and(|t| t < floor) {
            return false;
        }
        posts.extend(sid(&p["id"]));
        true
    })
    .await?;
    let fields = "id,message,from,created_time,parent{id},attachment{type,url,media}";
    for post in posts {
        let path = format!("{post}/comments");
        let query = [
            ("filter", "stream"),
            ("order", "reverse_chronological"),
            ("fields", fields),
            ("limit", "100"),
        ];
        read_pages(
            app,
            token,
            (&path, &query),
            (COMMENT_PAGES, deadline),
            |c| {
                if sid(&c["from"]["id"]).as_deref() == Some(page) {
                    out.answered.extend(sid(&c["parent"]["id"]));
                }
                let found = listed_facebook_comment(c, &post, page).filter(|f| f.at >= floor);
                out.items.extend(found);
                true
            },
        )
        .await?;
    }
    Ok(())
}

/// The comments of the last days on an Instagram account's media, replies included
/// (`instagram_basic`, `instagram_manage_comments`).
async fn instagram_comments(
    app: &AppState,
    (token, account, handle): (&str, &str, Option<&str>),
    (floor, deadline): (DateTime<FixedOffset>, Instant),
    out: &mut Found,
) -> Result<(), InboxError> {
    let mut media = Vec::new();
    let since = floor.timestamp().to_string();
    let path = format!("{account}/media");
    let query = [
        ("fields", "id,timestamp"),
        ("since", since.as_str()),
        ("limit", PAGE_SIZE),
    ];
    read_pages(app, token, (&path, &query), (POST_PAGES, deadline), |m| {
        media.extend(sid(&m["id"]));
        true
    })
    .await?;
    let reply = "id,text,timestamp,username,from{id,username}";
    let fields = format!("{reply},replies{{{reply}}}");
    for id in media {
        let path = format!("{id}/comments");
        let query = [("fields", fields.as_str()), ("limit", "50")];
        read_pages(
            app,
            token,
            (&path, &query),
            (COMMENT_PAGES, deadline),
            |c| {
                let top = sid(&c["id"]).unwrap_or_default();
                out.items.extend(listed_instagram_comment(c, &id, &id));
                for r in c["replies"]["data"].as_array().into_iter().flatten() {
                    let own = sid(&r["from"]["id"]).as_deref() == Some(account)
                        || handle.is_some_and(|h| {
                            let by = sid(&r["username"]).or_else(|| sid(&r["from"]["username"]));
                            by.is_some_and(|u| u.eq_ignore_ascii_case(h))
                        });
                    if own {
                        out.answered.insert(top.clone());
                    }
                    out.items.extend(listed_instagram_comment(r, &id, &top));
                }
                true
            },
        )
        .await?;
    }
    out.items.retain(|f| f.at >= floor);
    Ok(())
}

/// The recent messages of a Page's conversations (`pages_messaging`) or, with `instagram`, of its
/// linked Instagram account (`instagram_manage_messages`). Conversations come newest first, and
/// Graph has no time filter for them: the first one untouched since `floor` ends the list.
async fn conversations(
    app: &AppState,
    (token, page, me): (&str, &str, &str),
    (instagram, floor, deadline): (bool, DateTime<FixedOffset>, Instant),
    out: &mut Found,
) -> Result<(), InboxError> {
    let path = format!("{page}/conversations");
    let fields = format!(
        "updated_time,messages.limit({CONVERSATION_MESSAGES}){{id,message,from,created_time}}"
    );
    let mut query = vec![("fields", fields.as_str()), ("limit", PAGE_SIZE)];
    if instagram {
        query.push(("platform", "instagram"));
    }
    let pages = (CONVERSATION_PAGES, deadline);
    read_pages(app, token, (&path, &query), pages, |c| {
        if graph_time(&c["updated_time"]).is_some_and(|t| t < floor) {
            return false;
        }
        let ours = last_from(c, me);
        for m in listed_messages(c, me, floor) {
            if ours.is_some_and(|t| m.at <= t) {
                out.answered.insert(m.remote_id.clone());
            }
            out.items.push(m);
        }
        true
    })
    .await
}

/// Reads what `account` missed in the last `inbox::FIRST_DAYS` days and stores it; how many items
/// were new. `first`: the account was never read, so no automation may answer what is found. A section Meta refuses for a missing permission is logged and skipped, one that finds
/// the token dead flags the account for a reconnect and ends the read.
pub async fn backfill(
    app: &AppState,
    account: &social_accounts::Model,
    first: bool,
) -> Result<usize, DbErr> {
    let Some(token) = inbox::secret(app, account) else {
        return Ok(0);
    };
    let instagram = account.provider == "instagram";
    let page = if instagram {
        account.meta["page_id"].as_str().unwrap_or_default()
    } else {
        &account.external_id
    };
    let floor = (Utc::now() - Duration::days(inbox::FIRST_DAYS)).fixed_offset();
    let deadline = Instant::now() + BACKFILL_BUDGET;
    let mut found = Found::default();
    let id = account.external_id.as_str();
    let comments = if instagram {
        let keys = (token.as_str(), id, account.handle.as_deref());
        instagram_comments(app, keys, (floor, deadline), &mut found).await
    } else {
        facebook_comments(app, (token.as_str(), id), (floor, deadline), &mut found).await
    };
    let mut results = vec![comments];
    if !page.is_empty() {
        let who = (token.as_str(), page, account.external_id.as_str());
        let window = (instagram, floor, deadline);
        results.push(conversations(app, who, window, &mut found).await);
    }
    for r in results {
        match r {
            Ok(()) => {}
            Err(InboxError::MetaReconnect) => {
                let stmt = sql(
                    "UPDATE social_accounts SET inbox_error_at = now() WHERE id = $1",
                    vec![account.id.into()],
                );
                app.db.execute_raw(stmt).await?;
                crate::expiry::flag_reconnect(&app.db, account.id).await;
                break;
            }
            Err(e) => {
                tracing::warn!(code = e.code(), account = %account.id, "meta backfill read failed")
            }
        }
    }
    // Whatever the account itself wrote, by id (a comment's name may differ from its handle).
    let Found {
        mut items,
        answered,
    } = found;
    items.retain(|f| f.author.did != account.external_id);
    // Oldest first, as a webhook would have delivered them.
    items.sort_by_key(|f| f.at);
    let mut added = 0;
    let old = Utc::now() - Duration::hours(AUTOMATE_WITHIN_HOURS);
    for event in items {
        if is_ours(app, account, &event).await? || near_duplicate(app, account, &event).await? {
            continue;
        }
        // Older than the connection (nothing could have reached it), or answered by a person.
        let stale = first
            || event.at < old
            || event.at < account.created_at
            || answered.contains(&event.remote_id);
        // A late `stop` must not undo a newer `start` the webhook already stored.
        let opt = !newer_message(app, account, &event).await?;
        if inbox::store_with(&app.db, account, event, (opt, stale), app.billing_enabled)
            .await?
            .is_some()
        {
            added += 1;
        }
    }
    Ok(added)
}

/// Whether a DM with this text from this author is already stored within two minutes of it,
/// under another id (a message's id in a conversation listing is not guaranteed to be its
/// webhook `mid`).
async fn near_duplicate(
    app: &AppState,
    account: &social_accounts::Model,
    event: &Fresh,
) -> Result<bool, DbErr> {
    if event.kind != "dm" {
        return Ok(false);
    }
    let stmt = sql(
        "SELECT EXISTS (SELECT 1 FROM inbox_items WHERE social_account_id = $1 AND kind = 'dm' \
         AND author_id = $2 AND text = $3 AND received_at BETWEEN $4 - interval '2 minutes' \
         AND $4 + interval '2 minutes') AS ok",
        vec![
            account.id.into(),
            event.author.did.clone().into(),
            inbox::clip(&event.text).into(),
            event.at.into(),
        ],
    );
    match app.db.query_one_raw(stmt).await? {
        Some(r) => r.try_get("", "ok"),
        None => Ok(false),
    }
}

/// Whether the author of the DM `event` has a newer `stop`, `unsubscribe` or `start` stored for
/// this team, which the late `event` must not overrule. Any other newer message says nothing.
async fn newer_message(
    app: &AppState,
    account: &social_accounts::Model,
    event: &Fresh,
) -> Result<bool, DbErr> {
    if event.kind != "dm" {
        return Ok(false);
    }
    let stmt = sql(
        "SELECT EXISTS (SELECT 1 FROM inbox_items WHERE team_id = $1 AND platform = $2 \
         AND author_id = $3 AND kind = 'dm' AND received_at > $4 \
         AND lower(btrim(text)) IN ('stop', 'unsubscribe', 'start')) AS ok",
        vec![
            account.team_id.into(),
            account.provider.clone().into(),
            event.author.did.clone().into(),
            event.at.into(),
        ],
    );
    match app.db.query_one_raw(stmt).await? {
        Some(r) => r.try_get("", "ok"),
        None => Ok(false),
    }
}

/// One tick: rounds of due accounts, oldest read first, until none is due or `TICK_BUDGET` is
/// spent. A round claims at most `BACKFILL_CHUNK` accounts not read for an hour (or ever: a new
/// connection) by stamping them, so a second instance skips them and a crash costs one hour, and
/// reads them `BACKFILL_SLOTS` at a time. An account whose token is known dead, or whose read was
/// refused in the last hour, waits.
pub async fn tick(app: &AppState) -> Result<(), DbErr> {
    let deadline = Instant::now() + TICK_BUDGET;
    while round(app).await? == BACKFILL_CHUNK as usize && Instant::now() < deadline {}
    Ok(())
}

/// One chunk: claims it and reads it, returning how many accounts it claimed.
async fn round(app: &AppState) -> Result<usize, DbErr> {
    let claim = sql(
        "WITH due AS (SELECT id, meta_synced_at IS NULL AS first FROM social_accounts \
         WHERE provider IN ('facebook', 'instagram') \
         AND (meta_synced_at IS NULL OR meta_synced_at < now() - make_interval(mins => $1)) \
         AND (inbox_error_at IS NULL OR inbox_error_at < now() - make_interval(mins => $1)) \
         AND COALESCE(meta->>'reconnect', 'false') <> 'true' \
         ORDER BY meta_synced_at NULLS FIRST LIMIT $2 FOR UPDATE SKIP LOCKED) \
         UPDATE social_accounts a SET meta_synced_at = now() FROM due WHERE a.id = due.id \
         RETURNING a.id, due.first",
        vec![
            (BACKFILL_EVERY_MINUTES as i32).into(),
            BACKFILL_CHUNK.into(),
        ],
    );
    let rows = app.db.query_all_raw(claim).await?;
    let claimed = rows.len();
    let mut reads = Vec::new();
    for row in rows {
        let id: Uuid = row.try_get("", "id")?;
        let first: bool = row.try_get("", "first")?;
        let app = app.clone();
        reads.push(tokio::spawn(async move {
            let Ok(_slot) = SLOTS.acquire().await else {
                return;
            };
            let account = match social_accounts::Entity::find_by_id(id).one(&app.db).await {
                Ok(Some(a)) => a,
                Ok(None) => return,
                Err(e) => return tracing::error!(error = %e, account = %id, "meta backfill failed"),
            };
            match backfill(&app, &account, first).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(account = %id, items = n, "meta backfill stored items"),
                Err(e) => tracing::error!(error = %e, account = %id, "meta backfill failed"),
            }
        }));
    }
    for r in reads {
        let _ = r.await;
    }
    Ok(claimed)
}

/// Sends `text` for `item` from a Facebook or Instagram `account`, once: a public reply under
/// a comment, a private reply to a comment (a `Dm` for a comment: within 7 days, one per
/// comment), or a DM to a person who wrote within 24 hours. `buttons` go with a DM as quick
/// replies. `human`: a person sent it (not an automation), who may use the Human Agent tag. The
/// inner `Ok` is the sent id.
pub async fn send(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    via: Via,
    text: &str,
    (buttons, human): (&[Button], bool),
) -> Result<Result<String, InboxError>, DbErr> {
    let quick: Vec<(&str, &str)> = buttons
        .iter()
        .map(|b| (b.label.as_str(), b.payload.as_str()))
        .collect();
    let instagram = account.provider == "instagram";
    // Instagram messages go out through the linked Page, like Facebook ones.
    let page = if instagram {
        account.meta["page_id"].as_str().unwrap_or_default()
    } else {
        &account.external_id
    };
    let Some(token) = inbox::secret(app, account) else {
        return Ok(Err(InboxError::MetaReconnect));
    };
    if item.moderation["deleted"].as_bool() == Some(true) {
        return Ok(Err(InboxError::Invalid("not_repliable")));
    }
    let sent = match (via, item.kind.as_str()) {
        (Via::Public, "comment") => {
            app.meta
                .reply_to_comment(instagram, &token, &item.remote_id, text)
                .await
        }
        (Via::Dm, "comment") => {
            let late = item.received_at < (Utc::now() - Duration::days(PRIVATE_REPLY_DAYS));
            if late {
                Err(InboxError::Invalid("outside_window"))
            } else if private_reply_sent(app, item.id).await? {
                Err(InboxError::Invalid("already_replied"))
            } else {
                let to = To::Comment(&item.remote_id);
                app.meta
                    .message(page, &token, to, text, (&quick, false))
                    .await
            }
        }
        (Via::Dm, "dm") => {
            let last = last_message(app, account.id, &item.author_id).await?;
            let now = Utc::now();
            // 24 hours for anyone; a person's reply may use the Human Agent tag up to 7 days.
            let tagged = match last {
                Some(t) if t + Duration::hours(MESSAGE_HOURS) > now => Some(false),
                Some(t)
                    if human
                        && app.meta.human_agent
                        && t + Duration::days(HUMAN_AGENT_DAYS) > now =>
                {
                    Some(true)
                }
                _ => None,
            };
            match tagged {
                Some(tag) => {
                    let to = To::Person(&item.author_id);
                    app.meta
                        .message(page, &token, to, text, (&quick, tag))
                        .await
                }
                None => Err(InboxError::Invalid("outside_window")),
            }
        }
        _ => Err(InboxError::Invalid("not_repliable")),
    };
    if let Err(e) = &sent {
        flag_reconnect_if_dead(app, account, e).await;
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn page_comments_and_messages_become_items() {
        let entry = json!({"id": "101", "time": 1_700_000_000, "changes": [
            {"field": "feed", "value": {"item": "comment", "verb": "add",
                "comment_id": "101_9_5", "post_id": "101_9", "parent_id": "101_9",
                "from": {"id": "777", "name": "Ada"}, "message": "hi", "created_time": 1_700_000_100}},
            {"field": "feed", "value": {"item": "reaction", "verb": "add"}},
            {"field": "feed", "value": {"item": "comment", "verb": "edited", "comment_id": "1"}},
            {"field": "feed", "value": {"item": "comment", "verb": "add", "comment_id": "101_9_6",
                "post_id": "101_9", "parent_id": "101_9_5", "from": {"id": "101", "name": "Cafe"},
                "message": "thanks"}}],
            "messaging": [
            {"sender": {"id": "777"}, "recipient": {"id": "101"}, "timestamp": 1_700_000_200_000i64,
                "message": {"mid": "m1", "text": "hello"}},
            {"sender": {"id": "101"}, "recipient": {"id": "777"}, "message": {"mid": "m2", "text": "re", "is_echo": true}},
            {"sender": {"id": "777"}, "recipient": {"id": "101"}, "read": {"watermark": 1}},
            {"sender": {"id": "777"}, "recipient": {"id": "101"},
                "message": {"mid": "m3", "attachments": [{"type": "image"}]}},
            {"sender": {"id": "777"}, "recipient": {"id": "101"}, "message": {"mid": "m4"}}]});
        let got = events("page", &entry);
        let ids: Vec<_> = got.iter().map(|f| f.remote_id.as_str()).collect();
        assert_eq!(ids, ["101_9_5", "m1", "m3"]);
        let comment = &got[0];
        assert_eq!(comment.kind, "comment");
        assert_eq!(comment.parent.as_deref(), Some("101_9"));
        assert_eq!(comment.root.0.as_deref(), Some("101_9"));
        assert_eq!(comment.author.display_name.as_deref(), Some("Ada"));
        assert_eq!(comment.at.timestamp(), 1_700_000_100);
        assert_eq!(
            comment.permalink.as_deref(),
            Some("https://www.facebook.com/101_9")
        );
        assert_eq!(got[1].kind, "dm");
        assert_eq!(got[1].convo_id.as_deref(), Some("777"));
        assert_eq!(got[1].at.timestamp(), 1_700_000_200);
        assert_eq!(got[2].text, "[attachment]");
        assert!(got.iter().all(|f| f.story.is_none()));
    }

    #[test]
    fn story_replies_are_marked_dms() {
        let entry = json!({"id": "1784", "time": 1_700_000_000, "messaging": [
            {"sender": {"id": "555"}, "recipient": {"id": "1784"}, "timestamp": 1_700_000_000_000i64,
             "message": {"mid": "s1", "text": "love this",
                "reply_to": {"story": {"id": "st9", "url": "https://cdn.example/st9.jpg"}}}},
            {"sender": {"id": "555"}, "recipient": {"id": "1784"},
             "message": {"mid": "s2", "text": "plain", "reply_to": {"mid": "s1"}}}]});
        let got = events("instagram", &entry);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].kind, "dm");
        assert_eq!(
            got[0].story,
            Some((
                Some("st9".into()),
                Some("https://cdn.example/st9.jpg".into())
            ))
        );
        assert_eq!(got[1].story, None);
    }

    #[test]
    fn a_story_mention_is_a_dm_with_a_story_mention_attachment() {
        let entry = json!({"id": "1784", "time": 1_700_000_000, "messaging": [
            {"sender": {"id": "555"}, "recipient": {"id": "1784"}, "timestamp": 1_700_000_000_000i64,
             "message": {"mid": "sm1", "attachments": [
                {"type": "story_mention", "payload": {"url": "https://cdn.example/s.jpg"}}]}}]});
        let got = events("instagram", &entry);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "dm");
        let kinds: Vec<_> = got[0].attachments.iter().map(|a| a.kind.as_str()).collect();
        assert_eq!(kinds, ["story_mention"]);
    }

    #[test]
    fn instagram_comments_and_messages_become_items() {
        let entry = json!({"id": "1784", "time": 1_700_000_000, "changes": [
            {"field": "comments", "value": {"id": "c1", "text": "nice", "parent_id": "c0",
                "from": {"id": "555", "username": "jane"}, "media": {"id": "m9"}}},
            {"field": "comments", "value": {"id": "c2", "text": "mine",
                "from": {"id": "1784", "username": "cafe"}, "media": {"id": "m9"}}},
            {"field": "mentions", "value": {"media_id": "m9"}},
            {"field": "messages", "value": {"sender": {"id": "555"}, "recipient": {"id": "1784"},
                "timestamp": "1700000000", "message": {"mid": "d1", "text": "yo"}}}]});
        let got = events("instagram", &entry);
        let ids: Vec<_> = got.iter().map(|f| f.remote_id.as_str()).collect();
        assert_eq!(ids, ["c1", "d1"]);
        assert_eq!(got[0].parent.as_deref(), Some("c0"));
        assert_eq!(got[0].root.0.as_deref(), Some("m9"));
        assert_eq!(got[0].author.handle.as_deref(), Some("jane"));
        assert_eq!(got[0].at.timestamp(), 1_700_000_000);
        assert!(events("page", &entry).is_empty());
    }

    #[test]
    fn listed_comments_and_messages_become_items() {
        let c = json!({"id": "101_9_5", "message": "hi", "created_time": "2026-11-20T10:00:00+0000",
            "from": {"id": "777", "name": "Ada"}, "parent": {"id": "101_9_4"},
            "attachment": {"type": "photo", "media": {"image": {"src": "https://x.example/a.jpg"}}}});
        let got = listed_facebook_comment(&c, "101_9", "101").unwrap();
        assert_eq!(got.parent.as_deref(), Some("101_9_4"));
        assert_eq!(got.root.0.as_deref(), Some("101_9"));
        assert_eq!(got.author.display_name.as_deref(), Some("Ada"));
        assert_eq!(got.at.timestamp(), 1_795_168_800);
        assert_eq!(got.attachments.len(), 1);
        // A top-level comment answers the post; the Page's own and nameless ones are left out.
        let top =
            json!({"id": "1", "from": {"id": "7"}, "created_time": "2026-11-20T10:00:00+0000"});
        let top = listed_facebook_comment(&top, "101_9", "101").unwrap();
        assert_eq!(top.parent.as_deref(), Some("101_9"));
        let own =
            json!({"id": "2", "from": {"id": "101"}, "created_time": "2026-11-20T10:00:00+0000"});
        assert!(listed_facebook_comment(&own, "101_9", "101").is_none());
        let nameless = json!({"id": "3", "created_time": "2026-11-20T10:00:00+0000"});
        assert!(listed_facebook_comment(&nameless, "101_9", "101").is_none());

        let ig = json!({"id": "c1", "text": "nice", "timestamp": "2026-11-20T10:00:00+0000",
            "username": "jane", "from": {"id": "555"}});
        let got = listed_instagram_comment(&ig, "m9", "c0").unwrap();
        assert_eq!(got.parent.as_deref(), Some("c0"));
        assert_eq!(got.author.did, "555");
        assert_eq!(got.author.handle.as_deref(), Some("jane"));

        let floor = DateTime::parse_from_rfc3339("2026-11-19T00:00:00+00:00").unwrap();
        let convo = json!({"messages": {"data": [
            {"id": "m1", "message": "hello", "created_time": "2026-11-20T10:00:00+0000",
                "from": {"id": "777", "name": "Ada"}},
            {"id": "m2", "message": "ours", "created_time": "2026-11-20T10:01:00+0000", "from": {"id": "101"}},
            {"id": "m3", "message": "", "created_time": "2026-11-20T10:02:00+0000", "from": {"id": "777"}},
            {"id": "m4", "message": "old", "created_time": "2026-11-01T10:00:00+0000", "from": {"id": "777"}}]}});
        let got = listed_messages(&convo, "101", floor);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].remote_id, "m1");
        assert_eq!(got[0].convo_id.as_deref(), Some("777"));
    }
}
