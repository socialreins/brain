//! What a person does from a Bluesky inbox item beyond replying: like, repost, follow, mute,
//! block, hide a reply from our thread, delete our own reply, report. Each goes straight to the
//! account's PDS (or, for a report, Bluesky's moderation service through it), asks for a
//! confirmation first (`confirm: true`) and is logged in `inbox_actions`, whatever came of it.
//! Like, repost, follow and block check the live state first, so a second click adds nothing.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use unicode_segmentation::UnicodeSegmentation;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    connectors::bluesky::{Creds, InboxError},
    inbox,
    models::{inbox_items, inbox_replies, social_accounts},
    state::AppState,
};

const POST: &str = "app.bsky.feed.post";
const THREADGATE: &str = "app.bsky.feed.threadgate";
/// Bluesky's moderation service, which takes reports forwarded by the PDS.
const MOD_PROXY: &str = "did:plc:ar7c4by46qjdydhdevvrndac#atproto_labeler";
/// A threadgate holds at most this many hidden replies.
const MAX_HIDDEN: usize = 300;

#[derive(Serialize, Deserialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Like,
    Unlike,
    Repost,
    Unrepost,
    Follow,
    Unfollow,
    MuteAuthor,
    UnmuteAuthor,
    MuteThread,
    UnmuteThread,
    BlockAuthor,
    UnblockAuthor,
    HideReply,
    UnhideReply,
    DeleteReply,
    Report,
    // On a DM: the conversation (accept a request, leave it, mute it), its read mark, one
    // message (delete it for ourselves, react to it).
    AcceptConvo,
    LeaveConvo,
    MuteConvo,
    UnmuteConvo,
    MarkRead,
    DeleteMessage,
    React,
    Unreact,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Like => "like",
            Self::Unlike => "unlike",
            Self::Repost => "repost",
            Self::Unrepost => "unrepost",
            Self::Follow => "follow",
            Self::Unfollow => "unfollow",
            Self::MuteAuthor => "mute_author",
            Self::UnmuteAuthor => "unmute_author",
            Self::MuteThread => "mute_thread",
            Self::UnmuteThread => "unmute_thread",
            Self::BlockAuthor => "block_author",
            Self::UnblockAuthor => "unblock_author",
            Self::HideReply => "hide_reply",
            Self::UnhideReply => "unhide_reply",
            Self::DeleteReply => "delete_reply",
            Self::Report => "report",
            Self::AcceptConvo => "accept_convo",
            Self::LeaveConvo => "leave_convo",
            Self::MuteConvo => "mute_convo",
            Self::UnmuteConvo => "unmute_convo",
            Self::MarkRead => "mark_read",
            Self::DeleteMessage => "delete_message",
            Self::React => "react",
            Self::Unreact => "unreact",
        }
    }

    /// Acts on a DM's conversation or message.
    pub fn on_dm(self) -> bool {
        matches!(
            self,
            Self::AcceptConvo
                | Self::LeaveConvo
                | Self::MuteConvo
                | Self::UnmuteConvo
                | Self::MarkRead
                | Self::DeleteMessage
                | Self::React
                | Self::Unreact
        )
    }

    /// Owners and admins only: what is hard to take back or reaches Bluesky's moderators.
    pub fn manages(self) -> bool {
        matches!(
            self,
            Self::BlockAuthor
                | Self::UnblockAuthor
                | Self::HideReply
                | Self::UnhideReply
                | Self::DeleteReply
                | Self::Report
                | Self::LeaveConvo
                | Self::DeleteMessage
        )
    }

    /// Acts on the item's own post, so only a comment or mention has one.
    fn on_post(self) -> bool {
        matches!(
            self,
            Self::Like
                | Self::Unlike
                | Self::Repost
                | Self::Unrepost
                | Self::MuteThread
                | Self::UnmuteThread
                | Self::HideReply
                | Self::UnhideReply
        )
    }
}

/// `reason_type` of a report, as Bluesky names them without the `com.atproto.moderation.defs#reason` prefix.
pub const REPORT_REASONS: [(&str, &str); 6] = [
    ("spam", "com.atproto.moderation.defs#reasonSpam"),
    ("violation", "com.atproto.moderation.defs#reasonViolation"),
    ("misleading", "com.atproto.moderation.defs#reasonMisleading"),
    ("sexual", "com.atproto.moderation.defs#reasonSexual"),
    ("rude", "com.atproto.moderation.defs#reasonRude"),
    ("other", "com.atproto.moderation.defs#reasonOther"),
];

/// What the caller adds to an action.
#[derive(Default)]
pub struct Extra<'a> {
    /// `delete_reply`: the reply (an `inbox_replies` row of the item) to delete.
    pub reply: Option<&'a inbox_replies::Model>,
    /// `report`: `spam`, `violation`, `misleading`, `sexual`, `rude` or `other`.
    pub reason_type: Option<&'a str>,
    /// `report`: what the reporter says, up to 2000 characters.
    pub reason: Option<&'a str>,
    /// `report`: the account instead of the item's post.
    pub account: bool,
    /// `react`, `unreact`: one emoji.
    pub emoji: Option<&'a str>,
}

/// The account's own state toward the item's post and author, read live.
#[derive(Serialize, ToSchema, Default, Debug, PartialEq, Eq)]
pub struct Viewer {
    pub liked: bool,
    pub reposted: bool,
    pub following: bool,
    pub blocking: bool,
    pub muted: bool,
    pub thread_muted: bool,
}

/// An at:// URI split in repo, collection and record key.
fn parts(uri: &str) -> Option<(&str, &str, &str)> {
    let mut it = uri.strip_prefix("at://")?.splitn(3, '/');
    match (it.next(), it.next(), it.next()) {
        (Some(r), Some(c), Some(k))
            if ![r, c, k].iter().any(|p| p.is_empty() || p.contains('/')) =>
        {
            Some((r, c, k))
        }
        _ => None,
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn account_creds<'a>(account: &'a social_accounts::Model, secret: &'a str) -> Creds<'a> {
    Creds {
        pds: inbox::pds(account),
        did: &account.external_id,
        password: secret,
    }
}

async fn create(
    app: &AppState,
    c: &Creds<'_>,
    collection: &str,
    record: Value,
    rkey: Option<&str>,
) -> Result<String, InboxError> {
    // A record must say its own type.
    let mut record = record;
    record["$type"] = collection.into();
    let mut body = json!({ "repo": c.did, "collection": collection, "record": record });
    if let Some(k) = rkey {
        body["rkey"] = k.into();
    }
    let made = app
        .bluesky
        .xrpc(c, "com.atproto.repo.createRecord", &[], Some(&body))
        .await?;
    Ok(made["uri"].as_str().unwrap_or_default().to_owned())
}

/// Deletes the account's own record at `uri`; one that is gone counts as deleted.
async fn delete(app: &AppState, c: &Creds<'_>, uri: &str) -> Result<(), InboxError> {
    let Some((repo, collection, rkey)) = parts(uri).filter(|(repo, ..)| *repo == c.did) else {
        return Err(InboxError::Invalid("not_ours"));
    };
    let body = json!({ "repo": repo, "collection": collection, "rkey": rkey });
    match app
        .bluesky
        .xrpc(c, "com.atproto.repo.deleteRecord", &[], Some(&body))
        .await
    {
        // Deleting a record that is not there succeeds; a 400 is a real refusal.
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    }
}

/// The account's state toward the item's post (when it has one) and its author.
pub async fn viewer(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
) -> Result<Viewer, InboxError> {
    let Some(secret) = inbox::secret(app, account) else {
        return Err(InboxError::Reconnect);
    };
    let c = account_creds(account, &secret);
    state_of(app, &c, item, Read::Both).await.map(|(v, _)| v)
}

/// The viewer state with the record URIs behind it: like, repost, follow, block.
struct Uris {
    like: Option<String>,
    repost: Option<String>,
    follow: Option<String>,
    block: Option<String>,
}

/// What `state_of` reads: the post, the author, or both (an author Bluesky cannot show then
/// leaves their part empty instead of failing the rest).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Read {
    Post,
    Author,
    Both,
}

async fn state_of(
    app: &AppState,
    c: &Creds<'_>,
    item: &inbox_items::Model,
    read: Read,
) -> Result<(Viewer, Uris), InboxError> {
    let uri = |v: &Value| v.as_str().map(str::to_owned);
    let mut seen = Viewer::default();
    let mut uris = Uris {
        like: None,
        repost: None,
        follow: None,
        block: None,
    };
    if read != Read::Author && matches!(item.kind.as_str(), "comment" | "mention") {
        let posts = app.bluesky.posts(c, &[item.remote_id.as_str()]).await?;
        if let Some(p) = posts.first() {
            uris.like = uri(&p["viewer"]["like"]);
            uris.repost = uri(&p["viewer"]["repost"]);
            seen.thread_muted = p["viewer"]["threadMuted"] == true;
        }
    }
    let profile = if read == Read::Post {
        Value::Null
    } else {
        let q = [("actor", item.author_id.as_str())];
        match app
            .bluesky
            .xrpc(c, "app.bsky.actor.getProfile", &q, None)
            .await
        {
            Ok(p) => p,
            Err(InboxError::Refused { .. }) if read == Read::Both => Value::Null,
            Err(e) => return Err(e),
        }
    };
    let about = &profile["viewer"];
    uris.follow = uri(&about["following"]);
    uris.block = uri(&about["blocking"]);
    seen.muted = about["muted"] == true;
    (seen.liked, seen.reposted) = (uris.like.is_some(), uris.repost.is_some());
    (seen.following, seen.blocking) = (uris.follow.is_some(), uris.block.is_some());
    Ok((seen, uris))
}

/// Does `action` on `item`; the id of the record made, if one was. `Err(Invalid)` when the item
/// is no place for it (a DM has no post to like), before anything is sent.
pub async fn perform(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    action: Action,
    extra: &Extra<'_>,
) -> Result<Option<String>, InboxError> {
    if account.provider != "bluesky" || item.platform != "bluesky" {
        return Err(InboxError::Invalid("unsupported_platform"));
    }
    if item.author_id == account.external_id && action != Action::DeleteReply {
        return Err(InboxError::Invalid("own_account"));
    }
    let own_post = matches!(item.kind.as_str(), "comment" | "mention");
    if (action.on_post() && !own_post) || (action.on_dm() != (item.kind == "dm") && action.on_dm())
    {
        return Err(InboxError::Invalid("not_applicable"));
    }
    let Some(secret) = inbox::secret(app, account) else {
        return Err(InboxError::Reconnect);
    };
    let c = account_creds(account, &secret);
    let post = || -> Result<(&str, &str), InboxError> {
        let cid = item.remote_cid.as_deref();
        cid.map(|cid| (item.remote_id.as_str(), cid))
            .ok_or(InboxError::Invalid("not_applicable"))
    };
    let subject = |(uri, cid): (&str, &str)| json!({ "uri": uri, "cid": cid });
    let did = item.author_id.as_str();
    match action {
        Action::Like | Action::Repost | Action::Unlike | Action::Unrepost => {
            let (_, uris) = state_of(app, &c, item, Read::Post).await?;
            let (existing, collection) = match action {
                Action::Like | Action::Unlike => (uris.like, "app.bsky.feed.like"),
                _ => (uris.repost, "app.bsky.feed.repost"),
            };
            match (action, existing) {
                (Action::Like | Action::Repost, Some(uri)) => Ok(Some(uri)),
                (Action::Like | Action::Repost, None) => {
                    let record = json!({ "subject": subject(post()?), "createdAt": now() });
                    create(app, &c, collection, record, None).await.map(Some)
                }
                (_, Some(uri)) => delete(app, &c, &uri).await.map(|()| None),
                (_, None) => Ok(None),
            }
        }
        Action::Follow | Action::Unfollow | Action::BlockAuthor | Action::UnblockAuthor => {
            let (_, uris) = state_of(app, &c, item, Read::Author).await?;
            let (existing, collection) = match action {
                Action::Follow | Action::Unfollow => (uris.follow, "app.bsky.graph.follow"),
                _ => (uris.block, "app.bsky.graph.block"),
            };
            match (action, existing) {
                (Action::Follow | Action::BlockAuthor, Some(uri)) => Ok(Some(uri)),
                (Action::Follow | Action::BlockAuthor, None) => {
                    let record = json!({ "subject": did, "createdAt": now() });
                    create(app, &c, collection, record, None).await.map(Some)
                }
                (_, Some(uri)) => delete(app, &c, &uri).await.map(|()| None),
                (_, None) => Ok(None),
            }
        }
        Action::MuteAuthor | Action::UnmuteAuthor => {
            let method = if action == Action::MuteAuthor {
                "app.bsky.graph.muteActor"
            } else {
                "app.bsky.graph.unmuteActor"
            };
            let body = json!({ "actor": did });
            app.bluesky.xrpc(&c, method, &[], Some(&body)).await?;
            Ok(None)
        }
        Action::MuteThread | Action::UnmuteThread => {
            let method = if action == Action::MuteThread {
                "app.bsky.graph.muteThread"
            } else {
                "app.bsky.graph.unmuteThread"
            };
            let root = item.root_remote_id.as_deref().unwrap_or(&item.remote_id);
            let body = json!({ "root": root });
            app.bluesky.xrpc(&c, method, &[], Some(&body)).await?;
            Ok(None)
        }
        Action::HideReply | Action::UnhideReply => {
            let root = item.root_remote_id.as_deref().unwrap_or(&item.remote_id);
            let Some((repo, _, rkey)) = parts(root).filter(|(repo, ..)| *repo == c.did) else {
                return Err(InboxError::Invalid("not_our_thread"));
            };
            hide(
                app,
                &c,
                (root, repo, rkey),
                &item.remote_id,
                action == Action::HideReply,
            )
            .await
            .map(|()| None)
        }
        Action::DeleteReply => {
            let Some(reply) = extra.reply.filter(|r| r.kind == "public") else {
                return Err(InboxError::Invalid("not_applicable"));
            };
            let Some(uri) = reply
                .remote_id
                .as_deref()
                .filter(|u| parts(u).is_some_and(|(_, collection, _)| collection == POST))
            else {
                return Err(InboxError::Invalid("not_applicable"));
            };
            delete(app, &c, uri).await.map(|()| None)
        }
        Action::Report => {
            // A post is reported by its own uri and cid; only a comment or mention is one.
            let post = own_post.then(|| post().ok()).flatten();
            report(app, &c, item, extra, post).await.map(Some)
        }
        _ => dm_action(app, &c, item, action, extra).await.map(|()| None),
    }
}

/// A call on a DM's conversation or message through the chat service.
async fn dm_action(
    app: &AppState,
    c: &Creds<'_>,
    item: &inbox_items::Model,
    action: Action,
    extra: &Extra<'_>,
) -> Result<(), InboxError> {
    let Some(convo) = item.convo_id.as_deref() else {
        return Err(InboxError::Invalid("not_applicable"));
    };
    let message = item.remote_id.as_str();
    let (method, body) = match action {
        Action::AcceptConvo => ("acceptConvo", json!({ "convoId": convo })),
        Action::LeaveConvo => ("leaveConvo", json!({ "convoId": convo })),
        Action::MuteConvo => ("muteConvo", json!({ "convoId": convo })),
        Action::UnmuteConvo => ("unmuteConvo", json!({ "convoId": convo })),
        Action::MarkRead => (
            "updateRead",
            json!({ "convoId": convo, "messageId": message }),
        ),
        Action::DeleteMessage => (
            "deleteMessageForSelf",
            json!({ "convoId": convo, "messageId": message }),
        ),
        Action::React | Action::Unreact => {
            // Bluesky takes exactly one emoji.
            let Some(emoji) = extra
                .emoji
                .filter(|e| e.graphemes(true).count() == 1 && e.len() <= 64)
            else {
                return Err(InboxError::Invalid("invalid_emoji"));
            };
            let method = if action == Action::React {
                "addReaction"
            } else {
                "removeReaction"
            };
            (
                method,
                json!({ "convoId": convo, "messageId": message, "value": emoji }),
            )
        }
        _ => return Err(InboxError::Invalid("not_applicable")),
    };
    let method = format!("chat.bsky.convo.{method}");
    app.bluesky
        .xrpc(c, &method, &[], Some(&body))
        .await
        .map(|_| ())
}

/// A DM conversation as the account sees it now.
#[derive(Serialize, ToSchema, Debug, PartialEq, Eq)]
pub struct Convo {
    /// `accepted`, or `request` while the account has not accepted it (a message request).
    pub status: String,
    pub muted: bool,
    /// A group conversation: a reply goes to everyone in it.
    pub group: bool,
    /// Members Bluesky lists (a group lists only some of them).
    pub members: usize,
    pub unread: i64,
}

/// The DM conversation of `item`, read live.
pub async fn convo(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
) -> Result<Option<Convo>, InboxError> {
    let Some(id) = item.convo_id.as_deref() else {
        return Ok(None);
    };
    let Some(secret) = inbox::secret(app, account) else {
        return Err(InboxError::Reconnect);
    };
    let c = account_creds(account, &secret);
    let got = app
        .bluesky
        .xrpc(&c, "chat.bsky.convo.getConvo", &[("convoId", id)], None)
        .await?;
    let v = &got["convo"];
    Ok(Some(Convo {
        status: v["status"].as_str().unwrap_or("accepted").to_owned(),
        muted: v["muted"] == true,
        group: v["kind"]["$type"]
            .as_str()
            .is_some_and(|t| t.ends_with("groupConvo")),
        members: v["members"].as_array().map_or(0, Vec::len),
        unread: v["unreadCount"].as_i64().unwrap_or(0),
    }))
}

/// Adds `reply` to the hidden replies of the threadgate on `root` (a post of the account), or
/// takes it out. A thread without a gate gets one that still lets everyone reply.
async fn hide(
    app: &AppState,
    c: &Creds<'_>,
    (root, repo, rkey): (&str, &str, &str),
    reply: &str,
    hide: bool,
) -> Result<(), InboxError> {
    let q = [("repo", repo), ("collection", THREADGATE), ("rkey", rkey)];
    let found = match app
        .bluesky
        .xrpc(c, "com.atproto.repo.getRecord", &q, None)
        .await
    {
        Ok(found) => Some(found),
        Err(InboxError::Refused { status: 400, .. }) => None,
        Err(e) => return Err(e),
    };
    let mut record = found
        .as_ref()
        .map(|f| f["value"].clone())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({ "$type": THREADGATE, "post": root, "createdAt": now() }));
    let mut hidden: Vec<String> = record["hiddenReplies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|u| u.as_str().map(str::to_owned))
        .collect();
    let has = hidden.iter().any(|u| u == reply);
    match (hide, has) {
        (true, true) | (false, false) => return Ok(()),
        (true, false) if hidden.len() >= MAX_HIDDEN => {
            return Err(InboxError::Invalid("too_many_hidden"))
        }
        (true, false) => hidden.push(reply.to_owned()),
        (false, true) => hidden.retain(|u| u != reply),
    }
    record["hiddenReplies"] = json!(hidden);
    match found {
        None => create(app, c, THREADGATE, record, Some(rkey))
            .await
            .map(|_| ()),
        Some(f) => {
            let mut body = json!({ "repo": repo, "collection": THREADGATE, "rkey": rkey,
                "record": record });
            if let Some(cid) = f["cid"].as_str() {
                body["swapRecord"] = cid.into();
            }
            app.bluesky
                .xrpc(c, "com.atproto.repo.putRecord", &[], Some(&body))
                .await
                .map(|_| ())
        }
    }
}

/// Reports the item's post, or its author's account, to Bluesky's moderators; the report's id.
async fn report(
    app: &AppState,
    c: &Creds<'_>,
    item: &inbox_items::Model,
    extra: &Extra<'_>,
    post: Option<(&str, &str)>,
) -> Result<String, InboxError> {
    let Some(reason_type) = extra
        .reason_type
        .and_then(|t| REPORT_REASONS.iter().find(|(k, _)| *k == t))
        .map(|(_, v)| *v)
    else {
        return Err(InboxError::Invalid("invalid_reason"));
    };
    let subject = match (extra.account, post) {
        (false, Some((uri, cid))) => {
            json!({ "$type": "com.atproto.repo.strongRef", "uri": uri, "cid": cid })
        }
        // Nothing to report but the account, and it must be asked for.
        (false, None) => return Err(InboxError::Invalid("not_applicable")),
        (true, _) => json!({ "$type": "com.atproto.admin.defs#repoRef", "did": item.author_id }),
    };
    let mut body = json!({ "reasonType": reason_type, "subject": subject });
    if let Some(reason) = extra.reason.map(str::trim).filter(|r| !r.is_empty()) {
        body["reason"] = reason.into();
    }
    let made = app
        .bluesky
        .xrpc_via(
            c,
            "com.atproto.moderation.createReport",
            &[],
            Some(&body),
            Some(MOD_PROXY),
        )
        .await?;
    Ok(made["id"]
        .as_i64()
        .map(|n| n.to_string())
        .unwrap_or_default())
}

/// Records an attempt in `inbox_actions`; `by` is the member, none for an automation.
pub async fn log(
    app: &AppState,
    item: &inbox_items::Model,
    by: Option<Uuid>,
    action: Action,
    outcome: &Result<Option<String>, InboxError>,
    detail: Value,
) -> Result<(), sea_orm::DbErr> {
    use sea_orm::ConnectionTrait;
    let (status, remote, error) = match outcome {
        Ok(remote) => ("done", remote.clone(), None),
        Err(e) => ("failed", None, Some(e.code())),
    };
    let target = if action.on_post() || action == Action::Report {
        item.remote_id.clone()
    } else {
        item.author_id.clone()
    };
    let stmt = sea_orm::Statement::from_sql_and_values(
        sea_orm::DbBackend::Postgres,
        "INSERT INTO inbox_actions (id, team_id, inbox_item_id, social_account_id, action, \
         target, status, remote_id, detail, error, done_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        [
            Uuid::now_v7().into(),
            item.team_id.into(),
            item.id.into(),
            item.social_account_id.into(),
            action.as_str().into(),
            target.into(),
            status.into(),
            remote.into(),
            detail.into(),
            error.into(),
            by.into(),
        ],
    );
    app.db.execute_raw(stmt).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_uris_split_in_three_clean_parts() {
        assert_eq!(
            parts("at://did:plc:a/app.bsky.feed.post/3k"),
            Some(("did:plc:a", "app.bsky.feed.post", "3k"))
        );
        for bad in [
            "at://did:plc:a/app.bsky.feed.post",
            "at://a//k",
            "https://x/y/z",
            "at://a/b/c/d",
        ] {
            assert_eq!(parts(bad), None, "{bad}");
        }
    }

    #[test]
    fn what_reaches_moderators_or_is_hard_to_take_back_is_for_managers() {
        use Action::*;
        for a in [
            BlockAuthor,
            UnblockAuthor,
            HideReply,
            UnhideReply,
            DeleteReply,
            Report,
        ] {
            assert!(a.manages(), "{a:?}");
        }
        for a in [
            Like, Unlike, Repost, Unrepost, Follow, Unfollow, MuteAuthor, MuteThread,
        ] {
            assert!(!a.manages(), "{a:?}");
        }
        assert_eq!(
            serde_json::to_string(&MuteAuthor).unwrap(),
            "\"mute_author\""
        );
        assert_eq!(MuteAuthor.as_str(), "mute_author");
    }
}
