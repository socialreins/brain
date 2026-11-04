//! Facebook and Instagram in the inbox. `POST /webhooks/meta` (`controllers/meta_webhook.rs`)
//! hands each signed delivery to `ingest`, which stores comments (Page `feed`, Instagram
//! `comments`) and direct messages (`messaging`) of every connected account, across all teams,
//! into `inbox_items` with the same unique key as Bluesky, so a repeated delivery adds nothing.
//! `follow_up` then fills in what the webhook leaves out (a sender's name, an Instagram link)
//! and runs automations on new comments. Replies and DMs go out through `send`, once, never
//! retried.
use chrono::{DateTime, Duration, FixedOffset, Utc};
use sea_orm::ColumnTrait;
use sea_orm::{ConnectionTrait, DbBackend, DbErr, EntityTrait, QueryFilter, Statement};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    automations,
    connectors::{
        bluesky::{Author, InboxError},
        meta::To,
    },
    inbox::{self, Fresh, Via},
    models::{inbox_items, social_accounts},
    state::AppState,
};

/// A private reply to a comment works within 7 days of it.
const PRIVATE_REPLY_DAYS: i64 = 7;
/// A message to a person works within 24 hours of their last message.
const MESSAGE_HOURS: i64 = 24;

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
        author: Author {
            did: sid(&from["id"])?,
            display_name: sid(&from["name"]),
            ..Author::default()
        },
        text: v["message"].as_str().unwrap_or_default().to_owned(),
        at: at(v["created_time"].as_i64().or(entry_time), None),
        permalink: Some(format!("https://www.facebook.com/{post}")),
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
        author: Author {
            did: sid(&from["id"])?,
            handle: sid(&from["username"]),
            ..Author::default()
        },
        text: v["text"].as_str().unwrap_or_default().to_owned(),
        at: at(entry_time, None),
        permalink: None,
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
    let text = msg["text"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            let files = msg["attachments"].as_array()?;
            (!files.is_empty()).then(|| "[attachment]".to_owned())
        })?;
    Some(Fresh {
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

/// Stores what a signed delivery holds for every connected account it names, in any team; the
/// accounts and new items, for `follow_up`. Unknown accounts and items already stored are left
/// out.
pub async fn ingest(
    app: &AppState,
    payload: &Value,
) -> Result<Vec<(social_accounts::Model, inbox_items::Model)>, DbErr> {
    let object = payload["object"].as_str().unwrap_or_default();
    let provider = match object {
        "page" => "facebook",
        "instagram" => "instagram",
        _ => return Ok(Vec::new()),
    };
    let mut fresh = Vec::new();
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
                if let Some(item) = inbox::store(&app.db, &account, event.clone()).await? {
                    fresh.push((account.clone(), item));
                }
            }
        }
    }
    Ok(fresh)
}

fn sql(sql: &str, values: Vec<sea_orm::Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// After the webhook answered: fills in each new item, then runs automations on new comments.
/// Failures are logged; nothing here sends twice.
pub async fn follow_up(app: &AppState, fresh: Vec<(social_accounts::Model, inbox_items::Model)>) {
    for (account, item) in fresh {
        if let Err(e) = enrich_and_run(app, &account, item).await {
            tracing::error!(error = %e, account = %account.id, "meta inbox follow-up failed");
        }
    }
}

async fn enrich_and_run(
    app: &AppState,
    account: &social_accounts::Model,
    item: inbox_items::Model,
) -> Result<(), DbErr> {
    let instagram = account.provider == "instagram";
    if let Some(token) = inbox::secret(app, account) {
        if item.kind == "dm" {
            if let Some(p) = app.meta.profile(instagram, &token, &item.author_id).await {
                let stmt = sql(
                    "UPDATE inbox_items SET author_name = COALESCE($2, author_name), \
                     author_handle = COALESCE($3, author_handle), \
                     author_avatar = COALESCE($4, author_avatar) WHERE id = $1",
                    vec![
                        item.id.into(),
                        p.name.into(),
                        p.handle.into(),
                        p.avatar.into(),
                    ],
                );
                app.db.execute_raw(stmt).await?;
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
        }
    }
    if item.kind == "comment" {
        automations::run(app, account, &item).await?;
    }
    Ok(())
}

/// Whether the person wrote to the account within 24 hours, so a message may go to them.
async fn in_message_window(app: &AppState, account: Uuid, person: &str) -> Result<bool, DbErr> {
    let stmt = sql(
        "SELECT EXISTS (SELECT 1 FROM inbox_items WHERE social_account_id = $1 AND kind = 'dm' \
         AND author_id = $2 AND received_at > now() - make_interval(hours => $3)) AS ok",
        vec![account.into(), person.into(), (MESSAGE_HOURS as i32).into()],
    );
    match app.db.query_one_raw(stmt).await? {
        Some(r) => r.try_get("", "ok"),
        None => Ok(false),
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

/// Sends `text` for `item` from a Facebook or Instagram `account`, once: a public reply under
/// a comment, a private reply to a comment (a `Dm` for a comment: within 7 days, one per
/// comment), or a DM to a person who wrote within 24 hours. The inner `Ok` is the sent id.
pub async fn send(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    via: Via,
    text: &str,
) -> Result<Result<String, InboxError>, DbErr> {
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
                app.meta.message(page, &token, to, text).await
            }
        }
        (Via::Dm, "dm") => {
            if in_message_window(app, account.id, &item.author_id).await? {
                let to = To::Person(&item.author_id);
                app.meta.message(page, &token, to, text).await
            } else {
                Err(InboxError::Invalid("outside_window"))
            }
        }
        _ => Err(InboxError::Invalid("not_repliable")),
    };
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
}
