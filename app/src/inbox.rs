//! The inbox: comments, mentions and DMs of every connected account. Bluesky has no webhooks,
//! so a per-minute tick reads each account's notifications and DMs from its saved cursors and
//! stores new items (Meta posts into the same table from its webhook, `meta_inbox`). New items then run
//! the team's automations (`automations::run` picks the trigger from the item's kind). Replies, by people or automations, go out through `send`, at most
//! once: nothing here retries a send.
use std::{sync::LazyLock, time::Instant};

use chrono::{DateTime, Duration, FixedOffset, Utc};
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
    connectors::bluesky::{Author, Creds, InboxError, Message, Notification},
    meta_inbox,
    models::{inbox_items, inbox_replies, social_accounts},
    state::AppState,
};

/// A first read (or one after a long pause) goes back at most this far.
pub const FIRST_DAYS: i64 = 7;
/// Kinds an inbox filter takes. `follow` items are left out of item lists unless asked for.
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
/// After the platform rate limits an account's read, the tick leaves it alone this long.
const RETRY_MINUTES: i64 = 5;
/// Longest item text kept.
const MAX_TEXT_CHARS: usize = 10_000;
/// Bluesky chat limits a message to 1000 graphemes and 10000 bytes.
pub const DM_MAX_GRAPHEMES: usize = 1000;
const DM_MAX_BYTES: usize = 10_000;

/// Most attachments kept on one item.
pub const MAX_ATTACHMENTS: usize = 10;
/// Longest attachment link kept.
const MAX_URL_CHARS: usize = 2048;

/// A photo, video, sticker, file or link an item carries, as the platform linked it. Meta's
/// CDN links expire after a while, so a client shows them while they load and falls back to
/// the kind's label.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// `image`, `video`, `sticker`, `gif`, `audio`, `file` or `link`.
    pub kind: String,
    /// The file itself, or the page a `link` points to.
    pub url: String,
    /// A still to show for it (a video's thumbnail), when the platform gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_url: Option<String>,
}

/// `url` when it is an https link of a sane length; nothing else is stored or shown.
fn https(url: Option<&str>) -> Option<String> {
    let url = url?.trim();
    let parsed = reqwest::Url::parse(url).ok()?;
    let ok = parsed.scheme() == "https" && parsed.host_str().is_some();
    // The cap is on what is stored: percent-encoding can make the normalised form longer.
    let stored = parsed.to_string();
    (ok && stored.chars().count() <= MAX_URL_CHARS).then_some(stored)
}

impl Attachment {
    /// An attachment of `kind`, or `None` when `url` is not https. A preview that is not https
    /// is dropped.
    pub fn new(kind: &str, url: Option<&str>, preview: Option<&str>) -> Option<Self> {
        Some(Self {
            kind: kind.to_owned(),
            url: https(url)?,
            preview_url: https(preview),
        })
    }
}

/// Platforms whose accounts have an inbox.
pub const INBOX_PLATFORMS: [&str; 3] = ["bluesky", "facebook", "instagram"];
/// Most accounts one inbox holds.
pub const MAX_INBOX_ACCOUNTS: usize = 100;

/// What narrows an inbox beyond its accounts (`inbox_accounts`). Empty lists mean "any".
#[derive(Serialize, Deserialize, ToSchema, Default, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Filters {
    /// `comment`, `mention`, `dm`.
    #[serde(default)]
    pub kinds: Vec<String>,
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
        if !known(&self.kinds, &KINDS) {
            return Err("invalid_filters");
        }
        self.keywords = keywords(&self.keywords).ok_or("invalid_filters")?;
        self.kinds.sort();
        self.kinds.dedup();
        Ok(self)
    }
}

/// Most tags one contact holds.
pub const MAX_TAGS: usize = 20;

/// A contact tag, trimmed and lowercased: 1 to 30 characters, no commas, control characters or
/// characters that do not show (zero-width and text direction marks), so two tags that look the
/// same are the same.
pub fn tag(raw: &str) -> Option<String> {
    let t = raw.trim().to_lowercase();
    let hidden = |c: char| matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{FEFF}');
    let ok = (1..=30).contains(&t.chars().count())
        && !t.chars().any(|c| c == ',' || c.is_control() || hidden(c));
    ok.then_some(t)
}

/// Adds `tag` (one `tag` gave) to a contact: `None` when it was added, else why not
/// (`already_tagged`, `tag_limit`, or `no_contact` when the contact was deleted). The contact's row lock keeps adds at once under the cap.
pub async fn add_tag(
    db: &DatabaseConnection,
    contact: Uuid,
    tag: &str,
) -> Result<Option<&'static str>, DbErr> {
    let txn = db.begin().await?;
    let lock = sql(
        "SELECT 1 FROM contacts WHERE id = $1 FOR UPDATE",
        vec![contact.into()],
    );
    if txn.query_one_raw(lock).await?.is_none() {
        return Ok(Some("no_contact"));
    }
    let values: Vec<Value> = vec![contact.into(), tag.into(), (MAX_TAGS as i64).into()];
    let add = sql(
        "INSERT INTO contact_tags (contact_id, tag) SELECT c.id, $2 FROM contacts c \
         WHERE c.id = $1 AND (SELECT count(*) FROM contact_tags WHERE contact_id = $1) < $3 \
         ON CONFLICT DO NOTHING",
        values.clone(),
    );
    let added = txn.execute_raw(add).await?.rows_affected() == 1;
    let has = sql(
        "SELECT 1 FROM contact_tags WHERE contact_id = $1 AND tag = $2",
        values[..2].to_vec(),
    );
    let has = txn.query_one_raw(has).await?.is_some();
    txn.commit().await?;
    Ok(match (added, has) {
        (true, _) => None,
        (false, true) => Some("already_tagged"),
        (false, false) => Some("tag_limit"),
    })
}

/// `text` for a LIKE pattern, with `\`, `%` and `_` matching themselves.
pub fn escape_like(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// `$n` placeholders for `items`, appended to `values`.
pub(crate) fn placeholders(
    values: &mut Vec<Value>,
    items: impl IntoIterator<Item = Value>,
) -> String {
    let mut out = Vec::new();
    for v in items {
        values.push(v);
        out.push(format!("${}", values.len()));
    }
    out.join(", ")
}

/// An inbox as SQL conditions on `inbox_items i`, each starting with ` AND `, with their values
/// appended to `values` (numbered after those already there). `accounts_of` is a custom inbox's
/// id, whose accounts (`inbox_accounts`) hold its items; `None` for "All", which holds every one.
pub fn filter_sql(accounts_of: Option<Uuid>, f: &Filters, values: &mut Vec<Value>) -> String {
    let mut sql = String::new();
    if let Some(inbox) = accounts_of {
        let p = placeholders(values, [inbox.into()]);
        sql += &format!(
            " AND EXISTS (SELECT 1 FROM inbox_accounts ia WHERE ia.inbox_id = {p} \
             AND ia.social_account_id = i.social_account_id)"
        );
    }
    if !f.kinds.is_empty() {
        let p = placeholders(values, f.kinds.iter().map(|k| k.clone().into()));
        sql += &format!(" AND i.kind IN ({p})");
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

pub(crate) fn secret(app: &AppState, account: &social_accounts::Model) -> Option<String> {
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
        .filter(sea_orm::sea_query::Expr::cust(
            "(inbox_retry_at IS NULL OR inbox_retry_at < now())",
        ))
        .all(&app.db)
        .await?;
    let reads: Vec<_> = accounts
        .into_iter()
        .map(|a| {
            let app = app.clone();
            tokio::spawn(async move {
                let id = a.id;
                match store_new(&app, a).await {
                    Ok(stored) => stored,
                    Err(e) => {
                        tracing::error!(error = %e, account = %id, "inbox ingest failed");
                        None
                    }
                }
            })
        })
        .collect();
    let mut stored = Vec::new();
    for read in reads {
        if let Ok(Some(s)) = read.await {
            stored.push(s);
        }
    }
    // The lock covers reading and storing only: sends to the platforms, which can be slow, must
    // not hold up the next round. A second instance running these too sends nothing twice.
    lock.commit().await?;
    let runs: Vec<_> = stored
        .into_iter()
        .map(|(account, fresh)| {
            let app = app.clone();
            tokio::spawn(async move { run_fresh(&app, &account, &fresh).await })
        })
        .collect();
    for run in runs {
        let _ = run.await;
    }
    automations::sweep(app).await
}

/// Automations over one account's new items, oldest first. An item whose run fails keeps no
/// `automated_at`, so the sweep tries it again; the others still run.
async fn run_fresh(app: &AppState, account: &social_accounts::Model, fresh: &[inbox_items::Model]) {
    for item in fresh {
        if let Err(e) = automations::run(app, account, item).await {
            tracing::error!(error = %e, item = %item.id, "automations failed");
        }
    }
}

/// Reads one account's new notifications (follows included) and DMs, stores them, moves its
/// cursors and runs automations on the new items.
pub async fn ingest(app: &AppState, account: social_accounts::Model) -> Result<(), DbErr> {
    if let Some((account, fresh)) = store_new(app, account).await? {
        run_fresh(app, &account, &fresh).await;
    }
    Ok(())
}

/// What a failed read means for the account. A refused password pauses it for an hour and flags
/// it for reconnecting; a rate limit leaves it alone for `RETRY_MINUTES`. True when the round
/// should end there.
async fn read_failed(
    db: &DatabaseConnection,
    account: &social_accounts::Model,
    e: &InboxError,
) -> Result<bool, DbErr> {
    let stmt = match e {
        InboxError::Reconnect => sql(
            "UPDATE social_accounts SET inbox_error_at = now() WHERE id = $1",
            vec![account.id.into()],
        ),
        InboxError::RateLimited => sql(
            "UPDATE social_accounts SET inbox_retry_at = now() + make_interval(mins => $2) \
             WHERE id = $1",
            vec![account.id.into(), (RETRY_MINUTES as i32).into()],
        ),
        _ => return Ok(false),
    };
    db.execute_raw(stmt).await?;
    if *e == InboxError::Reconnect {
        crate::expiry::flag_reconnect(db, account.id).await;
    }
    Ok(true)
}

/// The storing half of `ingest`: the account as it now is and its new items, or `None` when
/// nothing was read (no slot, no secret, a refused token).
async fn store_new(
    app: &AppState,
    account: social_accounts::Model,
) -> Result<Option<(social_accounts::Model, Vec<inbox_items::Model>)>, DbErr> {
    let Ok(_slot) = ACCOUNT_SLOTS.acquire().await else {
        return Ok(None);
    };
    let db = &app.db;
    let Some(secret) = secret(app, &account) else {
        return Ok(None);
    };
    let c = Creds {
        pds: pds(&account),
        did: &account.external_id,
        password: &secret,
    };
    let floor = (Utc::now() - Duration::days(FIRST_DAYS)).fixed_offset();
    let started = Instant::now();
    let budget = app.bluesky.inbox_budget;
    let Some(row) = db
        .query_one_raw(sql(
            "SELECT notifications_cursor, notifications_pending_at, messages_resume_at \
             FROM social_accounts WHERE id = $1",
            vec![account.id.into()],
        ))
        .await?
    else {
        return Ok(None);
    };
    let cursor: Option<String> = row.try_get("", "notifications_cursor")?;
    let pending: Option<DateTime<FixedOffset>> = row.try_get("", "notifications_pending_at")?;
    let resume: Option<DateTime<FixedOffset>> = row.try_get("", "messages_resume_at")?;
    let since = account.notifications_seen_at.unwrap_or(floor).max(floor);
    let mut fresh = Vec::new();
    // Notifications get half the time, so DMs are read even behind a long backlog.
    let read = app
        .bluesky
        .notifications(&c, since, cursor.as_deref(), started + budget / 2)
        .await;
    match read {
        Ok(read) => {
            // Newest first, and the newest of a round that stops early is kept for its end.
            let newest = read.found.iter().map(|n| n.indexed_at).max().max(pending);
            // Oldest first, so automations see comments in the order they came.
            for n in read.found.iter().rev() {
                if n.author.did == account.external_id {
                    continue;
                }
                if let Some(item) = store_notification(db, &account, n).await? {
                    fresh.push(item);
                }
            }
            let stmt = match read.next {
                Some(next) => sql(
                    "UPDATE social_accounts SET notifications_cursor = $2, \
                     notifications_pending_at = $3 WHERE id = $1",
                    vec![account.id.into(), next.into(), newest.into()],
                ),
                None => sql(
                    "UPDATE social_accounts SET notifications_seen_at = COALESCE($2, \
                     notifications_seen_at), notifications_cursor = NULL, \
                     notifications_pending_at = NULL WHERE id = $1",
                    vec![account.id.into(), newest.into()],
                ),
            };
            db.execute_raw(stmt).await?;
            if let Some(e) = read.stopped {
                // What the earlier pages gave is stored and the cursor sits on the failed page.
                tracing::warn!(code = e.code(), account = %account.id, "notifications stopped, progress kept");
                if read_failed(db, &account, &e).await? {
                    return Ok(Some((account, fresh)));
                }
            }
        }
        Err(InboxError::Refused { status: 400, .. }) if cursor.is_some() => {
            // The saved page is gone: read from the newest again. What was stored stays, and
            // storing it twice adds nothing.
            tracing::warn!(account = %account.id, "notifications cursor refused, starting over");
            let stmt = sql(
                "UPDATE social_accounts SET notifications_cursor = NULL, \
                 notifications_pending_at = NULL WHERE id = $1",
                vec![account.id.into()],
            );
            db.execute_raw(stmt).await?;
        }
        Err(e) => {
            tracing::warn!(code = e.code(), account = %account.id, "notifications failed");
            if read_failed(db, &account, &e).await? {
                return Ok(Some((account, fresh)));
            }
        }
    }
    let msince = account.messages_seen_at.unwrap_or(floor).max(floor);
    let dm_access = match app
        .bluesky
        .messages(&c, msince, resume, started + budget)
        .await
    {
        Ok(read) => {
            for m in read.found.iter().rev() {
                if let Some(item) = store_message(db, &account, m).await? {
                    fresh.push(item);
                }
            }
            let stmt = match read.resume {
                Some(t) => sql(
                    "UPDATE social_accounts SET messages_resume_at = $2 WHERE id = $1",
                    vec![account.id.into(), t.into()],
                ),
                None => sql(
                    "UPDATE social_accounts SET messages_seen_at = COALESCE($2, \
                     messages_seen_at), messages_resume_at = NULL WHERE id = $1",
                    vec![account.id.into(), read.newest.into()],
                ),
            };
            db.execute_raw(stmt).await?;
            if let Some(e) = read.stopped {
                tracing::warn!(code = e.code(), account = %account.id, "messages stopped, progress kept");
                read_failed(db, &account, &e).await?;
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
            read_failed(db, &account, &e).await?;
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
    Ok(Some((account, fresh)))
}

pub(crate) fn clip(text: &str) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}

// The post a comment hangs off: our target's remote id is the comment's parent or root. A
// Facebook Reel's target keeps the video id instead, which `meta_inbox` looks up after the 200.
// The author's contact is upserted in the same statement, so an item never lacks one. The
// contact keeps the handle, name and avatar of its newest item (`$17`, when it came) that gave
// one, so an old item read late or delivered again changes none of them; it stores nothing.
const INSERT: &str = "WITH c AS (INSERT INTO contacts AS k (id, team_id, platform, external_id, \
    handle, display_name, avatar_url, first_seen_at, last_seen_at) \
    VALUES ($24, $2, $4, $12, $13, $14, $15, $17, $17) \
    ON CONFLICT (team_id, platform, external_id) DO UPDATE SET \
    handle = CASE WHEN $17 < k.last_seen_at THEN COALESCE(k.handle, $13) \
        ELSE COALESCE($13, k.handle) END, \
    display_name = CASE WHEN $17 < k.last_seen_at THEN COALESCE(k.display_name, $14) \
        ELSE COALESCE($14, k.display_name) END, \
    avatar_url = CASE WHEN $17 < k.last_seen_at THEN COALESCE(k.avatar_url, $15) \
        ELSE COALESCE($15, k.avatar_url) END, \
    first_seen_at = LEAST(k.first_seen_at, $17), \
    last_seen_at = GREATEST(k.last_seen_at, $17) RETURNING id) \
    INSERT INTO inbox_items (id, team_id, social_account_id, platform, kind, \
    remote_id, remote_cid, parent_remote_id, root_remote_id, root_remote_cid, convo_id, \
    author_id, author_handle, author_name, author_avatar, text, received_at, permalink, \
    story_reply, story_id, story_url, state, button_payload, attachments, post_id, contact_id, \
    automated_at) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, \
    $19, $20, $21, $22, $23, $25, (SELECT t.post_id FROM post_targets t WHERE t.social_account_id = $3 \
     AND t.remote_id IN ($8, $9) LIMIT 1), (SELECT id FROM c), \
     CASE WHEN $26 THEN now() END) \
    ON CONFLICT (social_account_id, remote_id) DO NOTHING RETURNING *";

/// An item to store, from any platform.
#[derive(Clone)]
pub struct Fresh {
    pub kind: &'static str,
    pub remote_id: String,
    pub remote_cid: Option<String>,
    /// The post or comment it answers.
    pub parent: Option<String>,
    /// The thread's first post, and its cid.
    pub root: (Option<String>, Option<String>),
    pub convo_id: Option<String>,
    pub author: Author,
    pub text: String,
    pub at: DateTime<FixedOffset>,
    pub permalink: Option<String>,
    /// For a DM replying to a story: the story's id and link, when given.
    pub story: Option<(Option<String>, Option<String>)>,
    /// For a DM that is a click on a quick reply button (Meta): the button's payload.
    pub button_payload: Option<String>,
    /// Kept up to `MAX_ATTACHMENTS`.
    pub attachments: Vec<Attachment>,
}

/// A button under a DM: a quick reply on Messenger and Instagram, whose click comes back as a
/// `button_clicked` item carrying `payload`. Bluesky has no buttons: the labels are appended
/// to the text as numbered lines, and an answer is an ordinary DM.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Button {
    /// 1 to 20 characters.
    pub label: String,
    /// 1 to 64 characters of `a-z`, `0-9`, `_` and `-`.
    pub payload: String,
}

/// `text` as it goes out where there are no buttons: the labels after it, one numbered line each.
pub fn numbered(text: &str, buttons: &[Button]) -> String {
    let mut out = text.to_owned();
    for (i, b) in buttons.iter().enumerate() {
        out.push_str(&format!("\n{}. {}", i + 1, b.label));
    }
    out
}

/// Stores `f` for `account`; the new item, or `None` when it was already there. The unique key
/// is the account and the platform's id, so a repeated read or webhook delivery adds nothing.
/// Its author's contact (one per team, platform and author id) is made or brought up to date in
/// the same statement, and the item points at it. A new DM that says `stop`, `unsubscribe` or
/// `start` then opts its author out of broadcasts or back in (`opt_out`, `contact_opt_outs`),
/// which only broadcasts read.
pub async fn store(
    db: &DatabaseConnection,
    account: &social_accounts::Model,
    f: Fresh,
) -> Result<Option<inbox_items::Model>, DbErr> {
    store_with(db, account, f, (true, false)).await
}

/// `store`; with `opt` false a `stop` or `start` changes nobody's broadcast opt-out (a message read
/// late, which a newer one from its author may already have overruled), and with `automated` the
/// item is stored as already automated, in the same statement, so no automation answers it.
pub async fn store_with(
    db: &DatabaseConnection,
    account: &social_accounts::Model,
    f: Fresh,
    (opt, automated): (bool, bool),
) -> Result<Option<inbox_items::Model>, DbErr> {
    let values: Vec<Value> = vec![
        Uuid::now_v7().into(),
        account.team_id.into(),
        account.id.into(),
        account.provider.clone().into(),
        f.kind.into(),
        f.remote_id.into(),
        f.remote_cid.into(),
        f.parent.into(),
        f.root.0.into(),
        f.root.1.into(),
        f.convo_id.into(),
        f.author.did.into(),
        f.author.handle.into(),
        f.author.display_name.into(),
        f.author.avatar.into(),
        clip(&f.text).into(),
        f.at.into(),
        f.permalink.into(),
        f.story.is_some().into(),
        f.story.clone().and_then(|s| s.0).into(),
        f.story.and_then(|s| s.1).into(),
        // A follow is not work to answer.
        (if f.kind == "follow" { "done" } else { "open" }).into(),
        f.button_payload.into(),
        Uuid::now_v7().into(),
        serde_json::json!(f.attachments[..f.attachments.len().min(MAX_ATTACHMENTS)]).into(),
        automated.into(),
    ];
    let item = inbox_items::Entity::find()
        .from_raw_sql(sql(INSERT, values))
        .one(db)
        .await?;
    let said = item.as_ref().filter(|_| opt);
    if let Some((i, out)) = said.and_then(|i| Some((i, opt_out(i)?))) {
        let person: Vec<Value> = vec![
            i.team_id.into(),
            i.platform.clone().into(),
            i.author_id.clone().into(),
        ];
        db.execute_raw(sql(if out { OPT_OUT } else { OPT_IN }, person))
            .await?;
    }
    Ok(item)
}

/// Opts a person (team, platform, the platform's id of them) out of broadcasts, or back in.
/// The row is the person's, not a contact's: it stays when the contact is deleted, so a contact
/// made again by a later item is still opted out.
pub const OPT_OUT: &str = "INSERT INTO contact_opt_outs (team_id, platform, external_id) \
    VALUES ($1, $2, $3) ON CONFLICT DO NOTHING";
pub const OPT_IN: &str =
    "DELETE FROM contact_opt_outs WHERE team_id = $1 AND platform = $2 AND external_id = $3";

/// What a new DM says about broadcasts: its whole text is `stop` or `unsubscribe` (opt out,
/// `true`) or `start` (back in), whatever the case. Anything else says nothing.
fn opt_out(item: &inbox_items::Model) -> Option<bool> {
    if item.kind != "dm" {
        return None;
    }
    match item.text.trim().to_lowercase().as_str() {
        "stop" | "unsubscribe" => Some(true),
        "start" => Some(false),
        _ => None,
    }
}

/// Stores a notification; the new item, or `None` when it was already there.
async fn store_notification(
    db: &DatabaseConnection,
    account: &social_accounts::Model,
    n: &Notification,
) -> Result<Option<inbox_items::Model>, DbErr> {
    let fresh = Fresh {
        kind: n.kind,
        remote_id: n.uri.clone(),
        remote_cid: n.cid.clone(),
        parent: n.parent_uri.clone(),
        root: (Some(n.root.0.clone()), n.root.1.clone()),
        convo_id: None,
        author: n.author.clone(),
        text: n.text.clone(),
        at: n.indexed_at,
        permalink: None,
        story: None,
        button_payload: None,
        attachments: n.attachments.clone(),
    };
    store(db, account, fresh).await
}

async fn store_message(
    db: &DatabaseConnection,
    account: &social_accounts::Model,
    m: &Message,
) -> Result<Option<inbox_items::Model>, DbErr> {
    let fresh = Fresh {
        kind: "dm",
        remote_id: m.id.clone(),
        remote_cid: None,
        parent: None,
        root: (None, None),
        convo_id: Some(m.convo_id.clone()),
        author: m.sender.clone(),
        text: m.text.clone(),
        at: m.sent_at,
        permalink: None,
        story: None,
        button_payload: None,
        attachments: Vec::new(),
    };
    store(db, account, fresh).await
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

/// `text` fits a reply on `platform`: Facebook takes comments of 8000 characters and messages of
/// 2000; Instagram comments of 2200 characters and messages of 1000 bytes; any other platform is
/// held to `fits`.
pub fn fits_on(platform: &str, via: Via, text: &str) -> bool {
    let chars = text.chars().count();
    let some = !text.trim().is_empty();
    match (platform, via) {
        ("facebook", Via::Public) => some && chars <= 8000,
        ("facebook", Via::Dm) => some && chars <= 2000,
        ("instagram", Via::Public) => some && chars <= 2200,
        ("instagram", Via::Dm) => some && text.len() <= 1000,
        _ => fits(via, text),
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
    let from = (sent_by, automation_id);
    send_with(app, account, item, via, text, skip, from, &[]).await
}

/// `send` with `buttons` under a DM (`from`: who sent it, a user or an automation): quick
/// replies on Facebook and Instagram, numbered lines after the text on Bluesky.
#[allow(clippy::too_many_arguments)]
pub async fn send_with(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    via: Via,
    text: &str,
    skip: &[std::ops::Range<usize>],
    (sent_by, automation_id): (Option<Uuid>, Option<Uuid>),
    buttons: &[Button],
) -> Result<Result<inbox_replies::Model, InboxError>, DbErr> {
    let meta = matches!(account.provider.as_str(), "facebook" | "instagram");
    let lined = numbered(text, buttons);
    // `skip` still holds: the lines come after everything it names.
    let text = if meta { text } else { &lined };
    if !meta && account.provider != "bluesky" {
        return Ok(Err(InboxError::Invalid("unsupported_platform")));
    }
    if !fits_on(&account.provider, via, text) {
        return Ok(Err(InboxError::Invalid("too_long")));
    }
    let sent = if meta {
        meta_inbox::send(app, account, item, via, text, buttons).await?
    } else {
        send_bluesky(app, account, item, (via, text, skip)).await
    };
    let remote_id = match sent {
        Ok(id) => id,
        Err(e) => {
            // A refused password is the account's state, not this send's: say so everywhere.
            if e == InboxError::Reconnect && !meta {
                crate::expiry::flag_reconnect(&app.db, account.id).await;
            }
            return Ok(Err(e));
        }
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

/// The Bluesky half of `send`: the id of what went out.
async fn send_bluesky(
    app: &AppState,
    account: &social_accounts::Model,
    item: &inbox_items::Model,
    (via, text, skip): (Via, &str, &[std::ops::Range<usize>]),
) -> Result<String, InboxError> {
    if via == Via::Dm && !account.dm_access {
        return Err(InboxError::NoDmAccess);
    }
    let Some(secret) = secret(app, account) else {
        return Err(InboxError::Reconnect);
    };
    let c = Creds {
        pds: pds(account),
        did: &account.external_id,
        password: &secret,
    };
    match via {
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
                    app.bluesky.can_reply(&c, parent.0).await?;
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn filters_become_numbered_sql() {
        let inbox = Uuid::now_v7();
        let f = Filters {
            kinds: vec!["comment".into(), "dm".into()],
            keywords: vec!["50%_off".into(), "Guide".into()],
            unanswered: true,
        };
        let mut values: Vec<Value> = vec![Uuid::nil().into()];
        let got = filter_sql(Some(inbox), &f, &mut values);
        assert!(got.starts_with(
            " AND EXISTS (SELECT 1 FROM inbox_accounts ia WHERE ia.inbox_id = $2 \
             AND ia.social_account_id = i.social_account_id)"
        ));
        assert!(got.contains(" AND i.kind IN ($3, $4)"));
        assert!(got.contains(" AND (i.text ILIKE $5 OR i.text ILIKE $6)"));
        assert!(got.ends_with(" AND i.replied_at IS NULL"));
        assert_eq!(values.len(), 6);
        assert_eq!(values[1], Value::from(inbox));
        assert_eq!(values[4], Value::from("%50\\%\\_off%".to_owned()));
        // "All" holds every account.
        let mut values = Vec::new();
        assert_eq!(filter_sql(None, &Filters::default(), &mut values), "");
        assert!(values.is_empty());
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
                keywords: vec!["  ".into()],
                ..Filters::default()
            },
        ] {
            assert_eq!(bad.checked(), Err("invalid_filters"));
        }
        for unknown in [json!({"kind": ["dm"]}), json!({"account_ids": []})] {
            assert!(serde_json::from_value::<Filters>(unknown).is_err());
        }
    }

    #[test]
    fn meta_replies_have_their_own_limits() {
        assert!(fits_on("facebook", Via::Public, &"a".repeat(8000)));
        assert!(!fits_on("facebook", Via::Public, &"a".repeat(8001)));
        assert!(fits_on("facebook", Via::Dm, &"a".repeat(2000)));
        assert!(!fits_on("facebook", Via::Dm, &"a".repeat(2001)));
        assert!(fits_on("instagram", Via::Public, &"a".repeat(2200)));
        assert!(!fits_on("instagram", Via::Public, &"a".repeat(2201)));
        assert!(fits_on("instagram", Via::Dm, &"a".repeat(1000)));
        assert!(!fits_on("instagram", Via::Dm, &"a".repeat(1001)));
        assert!(!fits_on("instagram", Via::Dm, " "));
        assert!(!fits_on("bluesky", Via::Public, &"a".repeat(301)));
    }

    #[test]
    fn tags_are_trimmed_lowercased_and_checked() {
        assert_eq!(tag("  VIP Lead "), Some("vip lead".into()));
        assert_eq!(tag(&"a".repeat(30)), Some("a".repeat(30)));
        let hidden = [
            "vi\u{200B}p",
            "\u{202E}vip",
            "v\u{2060}ip",
            "vip\u{FEFF}x",
            "a\u{200F}b",
        ];
        for bad in hidden {
            assert_eq!(tag(bad), None, "{bad:?}");
        }
        for bad in ["", "  ", "a,b", "a\tb", &"a".repeat(31)] {
            assert_eq!(tag(bad), None, "{bad:?}");
        }
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
