//! The inbox: comments, mentions and DMs of every connected account. Bluesky has no webhooks,
//! so a per-minute tick reads each account's notifications and DMs from its saved cursors and
//! stores new items (Meta will post into the same table from a webhook). New comments then run
//! the team's automations. Replies, by people or automations, go out through `send`, at most
//! once: nothing here retries a send.
use std::sync::LazyLock;

use chrono::{Duration, Utc};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, DbErr, EntityTrait, QueryFilter,
    Statement, TransactionTrait, Value,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use unicode_segmentation::UnicodeSegmentation;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    automations,
    connectors::bluesky::{Creds, InboxError, Message, Notification},
    models::{inbox_items, inbox_replies, social_accounts},
    state::AppState,
};

/// A first read (or one after a long pause) goes back at most this far.
pub const FIRST_DAYS: i64 = 7;
pub const KINDS: [&str; 3] = ["comment", "mention", "dm"];
pub const PLATFORMS: [&str; 8] = [
    "facebook",
    "instagram",
    "threads",
    "linkedin",
    "bluesky",
    "x",
    "tiktok",
    "youtube",
];
const INBOX_LOCK: i64 = 0x0049_4e42_4f58_5449;
/// Accounts read at once across the process.
static ACCOUNT_SLOTS: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(8));
/// After the platform refuses the stored password, ingest leaves the account alone this long,
/// so a revoked password is not tried every minute.
const PAUSE_MINUTES: i64 = 60;
/// Longest item text kept.
const MAX_TEXT_CHARS: usize = 10_000;
/// Bluesky chat limits a message to 1000 graphemes and 10000 bytes.
pub const DM_MAX_GRAPHEMES: usize = 1000;
const DM_MAX_BYTES: usize = 10_000;

/// Which items an inbox shows. Empty lists mean "any". At most one of `social_set_id` and
/// `account_ids`.
#[derive(Serialize, Deserialize, ToSchema, Default, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Filters {
    /// Accounts in this live set of the team.
    #[serde(default)]
    pub social_set_id: Option<Uuid>,
    /// These accounts of the team, at most 100.
    #[serde(default)]
    pub account_ids: Vec<Uuid>,
    /// `comment`, `mention`, `dm`.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// e.g. `bluesky`.
    #[serde(default)]
    pub platforms: Vec<String>,
    /// Any of these, case-insensitive, in the text; at most 20 of 1 to 100 characters.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Only items nobody replied to yet.
    #[serde(default)]
    pub unanswered: bool,
}

/// Trimmed, deduplicated keywords, or `None` when one is empty, too long, or there are too many.
pub fn keywords(raw: &[String]) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for k in raw {
        let k = k.trim();
        if k.is_empty() || k.chars().count() > 100 || k.chars().any(char::is_control) {
            return None;
        }
        if !out.iter().any(|o| o.to_lowercase() == k.to_lowercase()) {
            out.push(k.to_owned());
        }
    }
    (out.len() <= 20).then_some(out)
}

impl Filters {
    /// The filters, normalized, or the error code.
    pub fn checked(mut self) -> Result<Self, &'static str> {
        let known =
            |list: &[String], allowed: &[&str]| list.iter().all(|k| allowed.contains(&k.as_str()));
        if !known(&self.kinds, &KINDS) || !known(&self.platforms, &PLATFORMS) {
            return Err("invalid_filters");
        }
        if self.account_ids.len() > 100
            || (self.social_set_id.is_some() && !self.account_ids.is_empty())
        {
            return Err("invalid_filters");
        }
        self.keywords = keywords(&self.keywords).ok_or("invalid_filters")?;
        self.kinds.dedup();
        self.platforms.dedup();
        self.account_ids.sort();
        self.account_ids.dedup();
        Ok(self)
    }
}

/// `text` for a LIKE pattern, with `\`, `%` and `_` matching themselves.
fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// `$n` placeholders for `items`, appended to `values`.
fn placeholders(values: &mut Vec<Value>, items: impl IntoIterator<Item = Value>) -> String {
    let mut out = Vec::new();
    for v in items {
        values.push(v);
        out.push(format!("${}", values.len()));
    }
    out.join(", ")
}

/// `filters` as SQL conditions on `inbox_items i`, each starting with ` AND `, with their
/// values appended to `values` (numbered after those already there).
pub fn filter_sql(f: &Filters, values: &mut Vec<Value>) -> String {
    let mut sql = String::new();
    if let Some(set) = f.social_set_id {
        let p = placeholders(values, [set.into()]);
        sql += &format!(
            " AND i.social_account_id IN (SELECT m.social_account_id \
             FROM social_account_social_set m JOIN social_sets s ON s.id = m.social_set_id \
             WHERE m.social_set_id = {p} AND s.deleted_at IS NULL)"
        );
    }
    if !f.account_ids.is_empty() {
        let p = placeholders(values, f.account_ids.iter().map(|&a| a.into()));
        sql += &format!(" AND i.social_account_id IN ({p})");
    }
    if !f.kinds.is_empty() {
        let p = placeholders(values, f.kinds.iter().map(|k| k.clone().into()));
        sql += &format!(" AND i.kind IN ({p})");
    }
    if !f.platforms.is_empty() {
        let p = placeholders(values, f.platforms.iter().map(|k| k.clone().into()));
        sql += &format!(" AND i.platform IN ({p})");
    }
    if !f.keywords.is_empty() {
        let any: Vec<String> = f
            .keywords
            .iter()
            .map(|k| {
                let p = placeholders(values, [format!("%{}%", escape_like(k)).into()]);
                format!("i.text ILIKE {p}")
            })
            .collect();
        sql += &format!(" AND ({})", any.join(" OR "));
    }
    if f.unanswered {
        sql += " AND i.replied_at IS NULL";
    }
    sql
}

/// Accounts and set ids from a filter or condition: every account is the team's, and the set
/// a live set of the team.
pub async fn owned(
    db: &impl ConnectionTrait,
    team_id: Uuid,
    set: Option<Uuid>,
    accounts: &[Uuid],
) -> Result<bool, DbErr> {
    if let Some(set) = set {
        let live = crate::models::social_sets::Entity::find_by_id(set)
            .filter(crate::models::social_sets::Column::TeamId.eq(team_id))
            .filter(crate::models::social_sets::Column::DeletedAt.is_null())
            .one(db)
            .await?;
        if live.is_none() {
            return Ok(false);
        }
    }
    if accounts.is_empty() {
        return Ok(true);
    }
    let found = social_accounts::Entity::find()
        .filter(social_accounts::Column::TeamId.eq(team_id))
        .filter(social_accounts::Column::Id.is_in(accounts.iter().copied()))
        .all(db)
        .await?;
    Ok(found.len() == accounts.len())
}

fn sql(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

fn pds(account: &social_accounts::Model) -> &str {
    account.meta["pds"].as_str().unwrap_or_default()
}

fn secret(app: &AppState, account: &social_accounts::Model) -> Option<String> {
    app.cipher
        .decrypt(&account.access_token)
        .and_then(|b| String::from_utf8(b).ok())
}

/// One ingest round: every Bluesky account not paused, 8 at a time. Skips while another
/// instance holds the lock.
pub async fn tick(app: &AppState) -> Result<(), DbErr> {
    let lock = app.db.begin().await?;
    let row = lock
        .query_one_raw(sql(
            "SELECT pg_try_advisory_xact_lock($1) AS ok",
            vec![INBOX_LOCK.into()],
        ))
        .await?;
    let locked: bool = match row {
        Some(r) => r.try_get("", "ok")?,
        None => false,
    };
    if !locked {
        return Ok(());
    }
    if let Err(e) = automations::recover_stuck(&app.db).await {
        tracing::warn!(error = %e, "stuck automation run recovery failed");
    }
    let paused = (Utc::now() - Duration::minutes(PAUSE_MINUTES)).fixed_offset();
    let accounts = social_accounts::Entity::find()
        .filter(social_accounts::Column::Provider.eq("bluesky"))
        .filter(
            sea_orm::Condition::any()
                .add(social_accounts::Column::InboxErrorAt.is_null())
                .add(social_accounts::Column::InboxErrorAt.lt(paused)),
        )
        .all(&app.db)
        .await?;
    let runs: Vec<_> = accounts
        .into_iter()
        .map(|a| {
            let app = app.clone();
            tokio::spawn(async move {
                let id = a.id;
                if let Err(e) = ingest(&app, a).await {
                    tracing::error!(error = %e, account = %id, "inbox ingest failed");
                }
            })
        })
        .collect();
    for run in runs {
        let _ = run.await;
    }
    lock.commit().await
}

/// Reads one account's new notifications and DMs, stores them, moves its cursors and runs
/// automations on the new comments.
pub async fn ingest(app: &AppState, account: social_accounts::Model) -> Result<(), DbErr> {
    let Ok(_slot) = ACCOUNT_SLOTS.acquire().await else {
        return Ok(());
    };
    let db = &app.db;
    let Some(secret) = secret(app, &account) else {
        return Ok(());
    };
    let c = Creds {
        pds: pds(&account),
        did: &account.external_id,
        password: &secret,
    };
    let floor = (Utc::now() - Duration::days(FIRST_DAYS)).fixed_offset();
    let since = account.notifications_seen_at.unwrap_or(floor).max(floor);
    let mut fresh = Vec::new();
    match app.bluesky.notifications(&c, since).await {
        Ok(list) => {
            let newest = list.iter().map(|n| n.indexed_at).max();
            // Oldest first, so automations see comments in the order they came.
            for n in list.iter().rev() {
                if n.author.did == account.external_id {
                    continue;
                }
                if let Some(item) = store_notification(db, &account, n).await? {
                    fresh.push(item);
                }
            }
            if let Some(t) = newest {
                let stmt = sql(
                    "UPDATE social_accounts SET notifications_seen_at = $2 WHERE id = $1",
                    vec![account.id.into(), t.into()],
                );
                db.execute_raw(stmt).await?;
            }
        }
        Err(InboxError::Reconnect) => {
            let stmt = sql(
                "UPDATE social_accounts SET inbox_error_at = now() WHERE id = $1",
                vec![account.id.into()],
            );
            db.execute_raw(stmt).await?;
            return Ok(());
        }
        Err(e) => tracing::warn!(code = e.code(), account = %account.id, "notifications failed"),
    }
    let msince = account.messages_seen_at.unwrap_or(floor).max(floor);
    let dm_access = match app.bluesky.messages(&c, msince).await {
        Ok((list, newest)) => {
            for m in list.iter().rev() {
                store_message(db, &account, m).await?;
            }
            if let Some(t) = newest {
                let stmt = sql(
                    "UPDATE social_accounts SET messages_seen_at = $2 WHERE id = $1",
                    vec![account.id.into(), t.into()],
                );
                db.execute_raw(stmt).await?;
            }
            true
        }
        // An outage (5xx) keeps what was known; only a refusal means no access.
        Err(
            InboxError::NoDmAccess
            | InboxError::Refused {
                status: 400..=403, ..
            },
        ) => false,
        Err(e) => {
            tracing::warn!(code = e.code(), account = %account.id, "messages failed");
            account.dm_access
        }
    };
    let account = if dm_access == account.dm_access {
        account
    } else {
        let stmt = sql(
            "UPDATE social_accounts SET dm_access = $2 WHERE id = $1",
            vec![account.id.into(), dm_access.into()],
        );
        db.execute_raw(stmt).await?;
        social_accounts::Model {
            dm_access,
            ..account
        }
    };
    // shortcut: automations run only on items stored in this round, so a crash between the
    // insert and the run skips them; sweep recent unrun comments if that ever matters.
    for item in fresh.iter().filter(|i| i.kind == "comment") {
        automations::run(app, &account, item).await?;
    }
    Ok(())
}

fn clip(text: &str) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}

const INSERT: &str = "INSERT INTO inbox_items (id, team_id, social_account_id, platform, kind, \
    remote_id, remote_cid, parent_remote_id, root_remote_id, root_remote_cid, convo_id, \
    author_id, author_handle, author_name, author_avatar, text, received_at, post_id) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, \
    (SELECT t.post_id FROM post_targets t WHERE t.social_account_id = $3 \
     AND t.remote_id IN ($8, $9) LIMIT 1)) \
    ON CONFLICT (social_account_id, remote_id) DO NOTHING RETURNING *";

/// Stores a notification; the new item, or `None` when it was already there.
async fn store_notification(
    db: &DatabaseConnection,
    account: &social_accounts::Model,
    n: &Notification,
) -> Result<Option<inbox_items::Model>, DbErr> {
    let values: Vec<Value> = vec![
        Uuid::now_v7().into(),
        account.team_id.into(),
        account.id.into(),
        account.provider.clone().into(),
        n.kind.into(),
        n.uri.clone().into(),
        n.cid.clone().into(),
        n.parent_uri.clone().into(),
        n.root.0.clone().into(),
        n.root.1.clone().into(),
        Option::<String>::None.into(),
        n.author.did.clone().into(),
        n.author.handle.clone().into(),
        n.author.display_name.clone().into(),
        n.author.avatar.clone().into(),
        clip(&n.text).into(),
        n.indexed_at.into(),
    ];
    inbox_items::Entity::find()
        .from_raw_sql(sql(INSERT, values))
        .one(db)
        .await
}

async fn store_message(
    db: &DatabaseConnection,
    account: &social_accounts::Model,
    m: &Message,
) -> Result<Option<inbox_items::Model>, DbErr> {
    let none = || Value::from(Option::<String>::None);
    let values: Vec<Value> = vec![
        Uuid::now_v7().into(),
        account.team_id.into(),
        account.id.into(),
        account.provider.clone().into(),
        "dm".into(),
        m.id.clone().into(),
        none(),
        none(),
        none(),
        none(),
        m.convo_id.clone().into(),
        m.sender.did.clone().into(),
        m.sender.handle.clone().into(),
        m.sender.display_name.clone().into(),
        m.sender.avatar.clone().into(),
        clip(&m.text).into(),
        m.sent_at.into(),
    ];
    inbox_items::Entity::find()
        .from_raw_sql(sql(INSERT, values))
        .one(db)
        .await
}

/// How a reply goes out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Via {
    /// A reply in the item's thread (comments and mentions).
    Public,
    /// A DM: in the item's conversation, or to the item's author.
    Dm,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Dm => "dm",
        }
    }
}

/// `text` fits a Bluesky post (`Public`) or chat message (`Dm`).
pub fn fits(via: Via, text: &str) -> bool {
    match via {
        Via::Public => socialreins_core::post::validate("bluesky", text, &[]).is_ok(),
        Via::Dm => {
            !text.trim().is_empty()
                && text.len() <= DM_MAX_BYTES
                && text.graphemes(true).count() <= DM_MAX_GRAPHEMES
        }
    }
}

/// Sends `text` for `item` from `account`, once, and records it on the item (a reply row and
/// `replied_at`). The inner `Err` says why nothing was sent.
/// `skip`: byte ranges of `text` filled in from someone else, which never become links.
#[allow(clippy::too_many_arguments)]
pub async fn send(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    via: Via,
    text: &str,
    skip: &[std::ops::Range<usize>],
    sent_by: Option<Uuid>,
    automation_id: Option<Uuid>,
) -> Result<Result<inbox_replies::Model, InboxError>, DbErr> {
    if account.provider != "bluesky" {
        return Ok(Err(InboxError::Invalid("unsupported_platform")));
    }
    if !fits(via, text) {
        return Ok(Err(InboxError::Invalid("too_long")));
    }
    if via == Via::Dm && !account.dm_access {
        return Ok(Err(InboxError::NoDmAccess));
    }
    let Some(secret) = secret(app, account) else {
        return Ok(Err(InboxError::Reconnect));
    };
    let c = Creds {
        pds: pds(account),
        did: &account.external_id,
        password: &secret,
    };
    let sent = match via {
        Via::Public => {
            let parent = item
                .remote_cid
                .as_deref()
                .map(|cid| (&*item.remote_id, cid));
            let root = item
                .root_remote_id
                .as_deref()
                .zip(item.root_remote_cid.as_deref());
            match (item.kind.as_str(), parent, root) {
                ("comment" | "mention", Some(parent), Some(root)) => {
                    app.bluesky.reply(&c, text, skip, parent, root).await
                }
                _ => Err(InboxError::Invalid("not_repliable")),
            }
        }
        Via::Dm => {
            let convo = item.convo_id.as_deref();
            app.bluesky
                .send_dm(&c, convo, &item.author_id, text, skip)
                .await
        }
    };
    let remote_id = match sent {
        Ok(id) => id,
        Err(e) => return Ok(Err(e)),
    };
    let reply = inbox_replies::ActiveModel {
        id: sea_orm::ActiveValue::Set(Uuid::now_v7()),
        inbox_item_id: sea_orm::ActiveValue::Set(item.id),
        kind: sea_orm::ActiveValue::Set(via.as_str().into()),
        text: sea_orm::ActiveValue::Set(text.to_owned()),
        remote_id: sea_orm::ActiveValue::Set(Some(remote_id).filter(|r| !r.is_empty())),
        sent_by: sea_orm::ActiveValue::Set(sent_by),
        automation_id: sea_orm::ActiveValue::Set(automation_id),
        created_at: sea_orm::ActiveValue::Set(Utc::now().fixed_offset()),
    };
    let reply = inbox_replies::Entity::insert(reply)
        .exec_with_returning(&app.db)
        .await?;
    let stmt = sql(
        "UPDATE inbox_items SET replied_at = now() WHERE id = $1",
        vec![item.id.into()],
    );
    app.db.execute_raw(stmt).await?;
    Ok(Ok(reply))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_become_numbered_sql() {
        let set = Uuid::now_v7();
        let f = Filters {
            social_set_id: Some(set),
            kinds: vec!["comment".into(), "dm".into()],
            platforms: vec!["bluesky".into()],
            keywords: vec!["50%_off".into(), "Guide".into()],
            unanswered: true,
            ..Filters::default()
        };
        let mut values: Vec<Value> = vec![Uuid::nil().into()];
        let got = filter_sql(&f, &mut values);
        assert!(got.contains("WHERE m.social_set_id = $2 AND s.deleted_at IS NULL)"));
        assert!(got.contains(" AND i.kind IN ($3, $4)"));
        assert!(got.contains(" AND i.platform IN ($5)"));
        assert!(got.contains(" AND (i.text ILIKE $6 OR i.text ILIKE $7)"));
        assert!(got.ends_with(" AND i.replied_at IS NULL"));
        assert_eq!(values.len(), 7);
        assert_eq!(values[5], Value::from("%50\\%\\_off%".to_owned()));
        let accounts = Filters {
            account_ids: vec![set, Uuid::nil()],
            ..Filters::default()
        };
        let mut values = Vec::new();
        assert_eq!(
            filter_sql(&accounts, &mut values),
            " AND i.social_account_id IN ($1, $2)"
        );
        assert_eq!(filter_sql(&Filters::default(), &mut values), "");
    }

    #[test]
    fn filters_are_checked_and_normalized() {
        let ok = Filters {
            kinds: vec!["mention".into()],
            keywords: vec![" Guide ".into(), "guide".into()],
            ..Filters::default()
        }
        .checked()
        .unwrap();
        assert_eq!(ok.keywords, ["Guide"]);
        for bad in [
            Filters {
                kinds: vec!["like".into()],
                ..Filters::default()
            },
            Filters {
                platforms: vec!["myspace".into()],
                ..Filters::default()
            },
            Filters {
                keywords: vec!["  ".into()],
                ..Filters::default()
            },
            Filters {
                social_set_id: Some(Uuid::nil()),
                account_ids: vec![Uuid::nil()],
                ..Filters::default()
            },
        ] {
            assert_eq!(bad.checked(), Err("invalid_filters"));
        }
        let unknown = serde_json::from_value::<Filters>(serde_json::json!({"kind": ["dm"]}));
        assert!(unknown.is_err());
    }

    #[test]
    fn dm_and_post_limits() {
        assert!(fits(Via::Public, &"a".repeat(300)));
        assert!(!fits(Via::Public, &"a".repeat(301)));
        assert!(fits(Via::Dm, &"a".repeat(1000)));
        assert!(!fits(Via::Dm, &"a".repeat(1001)));
        assert!(!fits(Via::Dm, " "));
    }
}
