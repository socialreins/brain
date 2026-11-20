//! Broadcasts: one DM to every reachable contact with a tag. A start (`start`) snapshots the
//! audience into `broadcast_recipients`; `tick` (every minute, own advisory lock) then claims up
//! to 20 pending recipients of each sending broadcast in one short transaction (`claim`, with
//! each send's hit of the daily cap), commits, and only then sends through `inbox::send`.
//! A recipient moves `pending` to `sending` (claimed) to `delivering` (committed right before
//! the send) to an end state, and never back once it was `delivering`, so nobody is sent a
//! broadcast twice: one cut off mid-send ends `failed: interrupted`, one only claimed goes back
//! to `pending` (`recover`). Never send to a recipient without claiming it, and never retry one.
//! A platform that refuses a send for anything but the recipient's own settings (a rate limit,
//! a block, an outage, a dead token) pauses the broadcast at once, as do 5 failed sends in a
//! row: `paused` waits for a person (`resume`), it never goes on by itself.
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use sea_orm::{
    ConnectionTrait, DatabaseConnection, DbBackend, DbErr, EntityTrait, Statement,
    TransactionTrait, Value,
};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    automations,
    connectors::bluesky::InboxError,
    expiry,
    inbox::{self, Via},
    models::{inbox_items, social_accounts},
    rate_limit::{daily_refund, reserve_in},
    state::AppState,
};

/// The platforms a broadcast can DM on.
pub const PLATFORMS: [&str; 3] = ["bluesky", "facebook", "instagram"];
/// Most recipients of one broadcast.
pub const MAX_RECIPIENTS: u64 = 1000;
/// Broadcasts a team starts in 24 hours.
pub const STARTS_PER_DAY: i64 = 3;
/// Broadcast messages a team sends in 24 hours; past it, recipients wait as `pending`.
pub const MESSAGES_PER_DAY: i64 = 1000;
/// Recipients claimed per sending broadcast per round.
const BATCH: i64 = 20;
/// This many sends of a broadcast failing in a row pause it.
const FAILS_IN_A_ROW: i64 = 5;
/// A recipient `sending` this long was cut off; far above `ROUND` plus any send's timeout.
const STUCK_MINUTES: i32 = 15;
const BROADCAST_LOCK: i64 = 0x0042_5243_5354_5449;
/// How long a round may start sends; recipients it did not get to go back to `pending`.
pub const ROUND: Duration = Duration::from_secs(45);

pub fn message_bucket(team: Uuid) -> String {
    format!("broadcast:team:{team}")
}

fn start_bucket(team: Uuid) -> String {
    format!("broadcast:start:team:{team}")
}

fn sql(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

// One row per contact on a platform that takes DMs: why they cannot be sent to right now
// (`reason`, null when they can) and the item the DM goes out for. The account is the one the
// contact last wrote to: on Facebook and Instagram by DM only, and only while that DM is under
// 24 hours old (Meta's standard messaging window, never a message tag); on Bluesky by DM,
// comment or mention. The DM answers the latest DM item of that account when there is one, so
// it shows in the inbox conversation. Nobody gets two broadcasts within 24 hours
// (`contacts.last_broadcast_at`, set right before a send).
const ROUTE: &str = "SELECT c.id AS contact_id, c.display_name, c.platform, \
    CASE WHEN o.team_id IS NOT NULL THEN 'opted_out' \
         WHEN c.last_broadcast_at > now() - interval '24 hours' THEN 'recently_messaged' \
         WHEN l.id IS NULL THEN 'no_conversation' \
         WHEN a.id IS NULL OR COALESCE(a.meta->>'reconnect', 'false') = 'true' \
              OR (c.platform = 'bluesky' AND NOT a.dm_access) THEN 'account_disconnected' \
         WHEN c.platform <> 'bluesky' AND l.received_at <= now() - interval '24 hours' \
              THEN 'outside_window' END AS reason, \
    COALESCE(d.id, l.id) AS item_id \
    FROM contacts c \
    LEFT JOIN contact_opt_outs o ON o.team_id = c.team_id AND o.platform = c.platform \
        AND o.external_id = c.external_id \
    LEFT JOIN LATERAL (SELECT i.id, i.social_account_id, i.received_at FROM inbox_items i \
        WHERE i.contact_id = c.id AND (i.kind = 'dm' \
            OR (c.platform = 'bluesky' AND i.kind IN ('comment', 'mention'))) \
        ORDER BY i.received_at DESC, i.id DESC LIMIT 1) l ON true \
    LEFT JOIN social_accounts a ON a.id = l.social_account_id AND a.team_id = c.team_id \
    LEFT JOIN LATERAL (SELECT i.id FROM inbox_items i WHERE i.contact_id = c.id \
        AND i.social_account_id = l.social_account_id AND i.kind = 'dm' \
        ORDER BY i.received_at DESC, i.id DESC LIMIT 1) d ON true \
    WHERE c.platform IN ('bluesky', 'facebook', 'instagram')";

/// `ROUTE` for a team's (`$1`) contacts with a tag (`$2`), on one platform (`$3`) or all (null).
fn audience_sql() -> String {
    format!(
        "{ROUTE} AND c.team_id = $1 AND ($3::text IS NULL OR c.platform = $3) \
         AND EXISTS (SELECT 1 FROM contact_tags t WHERE t.contact_id = c.id AND t.tag = $2)"
    )
}

#[derive(Serialize, ToSchema, Default, Debug, PartialEq, Eq)]
pub struct ByReason {
    pub opted_out: u64,
    /// A broadcast of the team went out to them within the last 24 hours.
    pub recently_messaged: u64,
    /// Facebook and Instagram: their last message is over 24 hours old.
    pub outside_window: u64,
    /// They never wrote a DM (Facebook, Instagram), or nothing at all (a Bluesky follower).
    pub no_conversation: u64,
    /// The account they wrote to needs a reconnect, or (Bluesky) has no DM access.
    pub account_disconnected: u64,
}

#[derive(Serialize, ToSchema, Debug, PartialEq, Eq)]
pub struct PlatformCount {
    pub platform: String,
    pub reachable: u64,
}

#[derive(Serialize, ToSchema, Debug, PartialEq, Eq)]
pub struct Sample {
    pub contact_id: Uuid,
    pub display_name: Option<String>,
    pub platform: String,
}

/// Who a broadcast would go to if it started now.
#[derive(Serialize, ToSchema, Default, Debug, PartialEq, Eq)]
pub struct Preview {
    /// Contacts with the tag on a platform that takes DMs (and the broadcast's, when it has
    /// one): `reachable` plus every `by_reason` count.
    pub total: u64,
    /// What `confirm_total` of a send must equal.
    pub reachable: u64,
    pub by_reason: ByReason,
    /// Each platform with a contact in `total`, by name.
    pub by_platform: Vec<PlatformCount>,
    /// Up to 5 reachable contacts, the latest seen first.
    pub sample: Vec<Sample>,
}

/// The audience of `tag` (and `platform`) in `team`, counted now. Nothing is sent or stored.
// shortcut: reads one row per tagged contact; count in SQL once a tag holds tens of thousands.
pub async fn preview(
    db: &impl ConnectionTrait,
    team: Uuid,
    tag: &str,
    platform: Option<&str>,
) -> Result<Preview, DbErr> {
    let stmt = sql(
        &format!("{} ORDER BY c.last_seen_at DESC, c.id DESC", audience_sql()),
        vec![team.into(), tag.into(), platform.map(str::to_owned).into()],
    );
    let mut p = Preview::default();
    let mut platforms: std::collections::BTreeMap<String, u64> = Default::default();
    for r in db.query_all_raw(stmt).await? {
        let platform: String = r.try_get("", "platform")?;
        let reason: Option<String> = r.try_get("", "reason")?;
        p.total += 1;
        let reachable = platforms.entry(platform.clone()).or_default();
        match reason.as_deref() {
            None => {
                *reachable += 1;
                p.reachable += 1;
                if p.sample.len() < 5 {
                    p.sample.push(Sample {
                        contact_id: r.try_get("", "contact_id")?,
                        display_name: r.try_get("", "display_name")?,
                        platform,
                    });
                }
            }
            Some("opted_out") => p.by_reason.opted_out += 1,
            Some("recently_messaged") => p.by_reason.recently_messaged += 1,
            Some("outside_window") => p.by_reason.outside_window += 1,
            Some("no_conversation") => p.by_reason.no_conversation += 1,
            Some(_) => p.by_reason.account_disconnected += 1,
        }
    }
    p.by_platform = platforms
        .into_iter()
        .map(|(platform, reachable)| PlatformCount {
            platform,
            reachable,
        })
        .collect();
    Ok(p)
}

/// What `start` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Start {
    Started,
    NotFound,
    /// It is no draft.
    AlreadyStarted,
    /// Another broadcast of the team is `sending`.
    AnotherSending,
    /// Nobody is reachable.
    NoRecipients,
    /// Over `MAX_RECIPIENTS`.
    TooManyRecipients,
    /// The reachable count is not the confirmed one.
    AudienceChanged,
    /// `STARTS_PER_DAY` reached.
    TooManyToday,
}

/// Starts a draft: snapshots everyone reachable right now into `broadcast_recipients` and sets
/// it `sending`, all in one transaction, and only when the snapshot holds exactly `confirm`
/// people. Starts of one team take turns (an advisory lock), so the one-at-a-time and
/// three-a-day rules hold; anything but `Started` leaves no trace.
pub async fn start(
    db: &DatabaseConnection,
    team: Uuid,
    id: Uuid,
    confirm: u64,
) -> Result<Start, DbErr> {
    let txn = db.begin().await?;
    let turn = sql(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
        vec![start_bucket(team).into()],
    );
    txn.execute_raw(turn).await?;
    let found = sql(
        "SELECT status, tag, platform FROM broadcasts WHERE id = $1 AND team_id = $2 FOR UPDATE",
        vec![id.into(), team.into()],
    );
    let Some(row) = txn.query_one_raw(found).await? else {
        return Ok(Start::NotFound);
    };
    if row.try_get::<String>("", "status")? != "draft" {
        return Ok(Start::AlreadyStarted);
    }
    let busy = sql(
        "SELECT 1 FROM broadcasts WHERE team_id = $1 AND status = 'sending'",
        vec![team.into()],
    );
    if txn.query_one_raw(busy).await?.is_some() {
        return Ok(Start::AnotherSending);
    }
    let (tag, platform): (String, Option<String>) =
        (row.try_get("", "tag")?, row.try_get("", "platform")?);
    // The snapshot is the count: one statement reads the audience and stores it.
    let snapshot = sql(
        &format!(
            "INSERT INTO broadcast_recipients (id, broadcast_id, contact_id) \
             SELECT gen_random_uuid(), $4, q.contact_id FROM ({}) q WHERE q.reason IS NULL",
            audience_sql()
        ),
        vec![team.into(), tag.into(), platform.into(), id.into()],
    );
    let n = txn.execute_raw(snapshot).await?.rows_affected();
    if n > MAX_RECIPIENTS {
        return Ok(Start::TooManyRecipients);
    }
    if n != confirm {
        return Ok(Start::AudienceChanged);
    }
    if n == 0 {
        return Ok(Start::NoRecipients);
    }
    let cap = [(start_bucket(team), STARTS_PER_DAY)];
    if reserve_in(&txn, &cap).await?.is_none() {
        return Ok(Start::TooManyToday);
    }
    let go = sql(
        "UPDATE broadcasts SET status = 'sending', started_at = now() WHERE id = $1",
        vec![id.into()],
    );
    txn.execute_raw(go).await?;
    txn.commit().await?;
    Ok(Start::Started)
}

/// Cancels a `sending` or `paused` broadcast: its pending recipients are `skipped: cancelled`.
/// `None` when the team has no such broadcast, `false` when it is neither. A recipient already
/// claimed ends on its own: `attempt` looks at the broadcast again before it sends.
pub async fn cancel(db: &DatabaseConnection, team: Uuid, id: Uuid) -> Result<Option<bool>, DbErr> {
    let txn = db.begin().await?;
    let key: Vec<Value> = vec![id.into(), team.into()];
    let stop = sql(
        "UPDATE broadcasts SET status = 'cancelled', pause_reason = NULL, finished_at = now() \
         WHERE id = $1 AND team_id = $2 AND status IN ('sending', 'paused')",
        key.clone(),
    );
    if txn.execute_raw(stop).await?.rows_affected() != 1 {
        let exists = sql(
            "SELECT 1 FROM broadcasts WHERE id = $1 AND team_id = $2",
            key,
        );
        return Ok(txn.query_one_raw(exists).await?.map(|_| false));
    }
    let skip = sql(
        "UPDATE broadcast_recipients SET status = 'skipped', reason = 'cancelled', \
         updated_at = now() WHERE broadcast_id = $1 AND status = 'pending'",
        vec![id.into()],
    );
    txn.execute_raw(skip).await?;
    txn.commit().await?;
    Ok(Some(true))
}

/// What `resume` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Resume {
    Resumed,
    NotFound,
    NotPaused,
    /// Another broadcast of the team is `sending`.
    AnotherSending,
}

/// Puts a `paused` broadcast back to `sending`, in the turn starts take, so one broadcast per
/// team sends. Its run of failures counts from now (`resumed_at`). It is no new start: the
/// three-a-day rule does not count it.
pub async fn resume(db: &DatabaseConnection, team: Uuid, id: Uuid) -> Result<Resume, DbErr> {
    let txn = db.begin().await?;
    let turn = sql(
        "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
        vec![start_bucket(team).into()],
    );
    txn.execute_raw(turn).await?;
    let found = sql(
        "SELECT status FROM broadcasts WHERE id = $1 AND team_id = $2 FOR UPDATE",
        vec![id.into(), team.into()],
    );
    let Some(row) = txn.query_one_raw(found).await? else {
        return Ok(Resume::NotFound);
    };
    if row.try_get::<String>("", "status")? != "paused" {
        return Ok(Resume::NotPaused);
    }
    let busy = sql(
        "SELECT 1 FROM broadcasts WHERE team_id = $1 AND status = 'sending'",
        vec![team.into()],
    );
    if txn.query_one_raw(busy).await?.is_some() {
        return Ok(Resume::AnotherSending);
    }
    let go = sql(
        "UPDATE broadcasts SET status = 'sending', pause_reason = NULL, resumed_at = now() \
         WHERE id = $1",
        vec![id.into()],
    );
    txn.execute_raw(go).await?;
    txn.commit().await?;
    Ok(Resume::Resumed)
}

/// Pauses a `sending` broadcast; `why` is for the person who must look at it.
async fn pause(db: &impl ConnectionTrait, broadcast: Uuid, why: &str) -> Result<(), DbErr> {
    let stmt = sql(
        "UPDATE broadcasts SET status = 'paused', pause_reason = $2 \
         WHERE id = $1 AND status = 'sending'",
        vec![broadcast.into(), why.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// Pauses every sending broadcast (or only `broadcast`) whose last `FAILS_IN_A_ROW` sends since
/// it started or was resumed all failed, whatever the reason; whether any was paused.
async fn pause_failing(db: &impl ConnectionTrait, broadcast: Option<Uuid>) -> Result<bool, DbErr> {
    let stmt = sql(
        "UPDATE broadcasts b SET status = 'paused', pause_reason = $2 \
         WHERE b.status = 'sending' AND ($3::uuid IS NULL OR b.id = $3) \
         AND (SELECT count(*) FROM (SELECT r.status FROM broadcast_recipients r \
              WHERE r.broadcast_id = b.id AND r.status IN ('sent', 'failed') \
              AND r.updated_at >= COALESCE(b.resumed_at, b.started_at) \
              ORDER BY r.updated_at DESC, r.id LIMIT $1) l WHERE l.status = 'failed') = $1",
        vec![
            FAILS_IN_A_ROW.into(),
            format!("{FAILS_IN_A_ROW} sends in a row failed.").into(),
            broadcast.into(),
        ],
    );
    Ok(db.execute_raw(stmt).await?.rows_affected() > 0)
}

/// A claimed recipient: `sending`, with its hit of the daily cap taken.
struct Claimed {
    id: Uuid,
    broadcast: Uuid,
    contact: Option<Uuid>,
    team: Uuid,
    tag: String,
    platform: Option<String>,
    text: String,
    created_by: Option<Uuid>,
    hits: Vec<i64>,
}

/// Hands the claimed (`sending`) recipients matching the `WHERE` appended to it back: nothing
/// was sent to them, so they are `pending` again, or `skipped: cancelled` when their broadcast
/// was cancelled meanwhile (a cancelled one never shows anybody pending).
const HAND_BACK: &str = "UPDATE broadcast_recipients r SET updated_at = now(), \
    status = CASE WHEN b.status = 'cancelled' THEN 'skipped' ELSE 'pending' END, \
    reason = CASE WHEN b.status = 'cancelled' THEN 'cancelled' END \
    FROM broadcasts b WHERE b.id = r.broadcast_id AND r.status = 'sending'";

/// What a process that died left behind. A recipient whose send was started (`delivering`)
/// fails as `interrupted` and is never sent to again; one only claimed (`sending`) got nothing,
/// so it is handed back.
async fn recover(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    let stale = "updated_at < now() - make_interval(mins => $1)";
    let cut = format!(
        "UPDATE broadcast_recipients SET status = 'failed', reason = 'interrupted', \
         updated_at = now() WHERE status = 'delivering' AND {stale}"
    );
    db.execute_raw(sql(&cut, vec![STUCK_MINUTES.into()]))
        .await?;
    let back = format!("{HAND_BACK} AND r.{stale}");
    db.execute_raw(sql(&back, vec![STUCK_MINUTES.into()]))
        .await
        .map(|_| ())
}

/// Moves up to `BATCH` pending recipients of every sending broadcast to `sending`, each with
/// its hit of the team's daily cap. Call it in a transaction that commits before any send:
/// `SKIP LOCKED` keeps two callers off the same recipient. At the cap the rest stay `pending`,
/// for a later round.
async fn claim(txn: &impl ConnectionTrait) -> Result<Vec<Claimed>, DbErr> {
    let sending = "SELECT id, team_id, tag, platform, text, created_by FROM broadcasts \
                   WHERE status = 'sending' ORDER BY started_at";
    let mut out = Vec::new();
    for b in txn.query_all_raw(sql(sending, vec![])).await? {
        let (broadcast, team): (Uuid, Uuid) = (b.try_get("", "id")?, b.try_get("", "team_id")?);
        let pending = sql(
            "SELECT id, contact_id FROM broadcast_recipients WHERE broadcast_id = $1 \
             AND status = 'pending' ORDER BY id LIMIT $2 FOR UPDATE SKIP LOCKED",
            vec![broadcast.into(), BATCH.into()],
        );
        let cap = [(message_bucket(team), MESSAGES_PER_DAY)];
        for r in txn.query_all_raw(pending).await? {
            let Some(hits) = reserve_in(txn, &cap).await? else {
                break;
            };
            let id: Uuid = r.try_get("", "id")?;
            let take = sql(
                "UPDATE broadcast_recipients SET status = 'sending', updated_at = now() \
                 WHERE id = $1",
                vec![id.into()],
            );
            txn.execute_raw(take).await?;
            out.push(Claimed {
                id,
                broadcast,
                team,
                contact: r.try_get("", "contact_id")?,
                tag: b.try_get("", "tag")?,
                platform: b.try_get("", "platform")?,
                text: b.try_get("", "text")?,
                created_by: b.try_get("", "created_by")?,
                hits,
            });
        }
    }
    Ok(out)
}

/// What became of a claimed recipient.
enum Tried {
    /// Its end state, and why when it was not sent.
    End(&'static str, Option<String>),
    /// The broadcast was paused meanwhile: nothing was sent, it is `pending` again.
    Back,
    /// The claim is no longer ours (`recover` took it for dead): nothing was sent, and the row
    /// is somebody else's now.
    Lost,
    /// The platform refused the send for more than this one person: the recipient is `failed`
    /// with the code, and the broadcast pauses with what the platform said.
    Trip(&'static str, String),
}

fn skipped(reason: &str) -> Tried {
    Tried::End("skipped", Some(reason.to_owned()))
}

/// Sends to one claimed recipient, once. Everything a start checked is checked again first,
/// with the start's own audience clause: the broadcast still sends, the contact is there, still
/// has the tag, did not opt out and got no other broadcast today, the account is connected,
/// and (Facebook, Instagram) the 24 hours still hold.
async fn attempt(app: &AppState, c: &Claimed) -> Result<Tried, DbErr> {
    let db = &app.db;
    let live = sql(
        "SELECT status FROM broadcasts WHERE id = $1",
        vec![c.broadcast.into()],
    );
    let status: Option<String> = match db.query_one_raw(live).await? {
        Some(r) => r.try_get("", "status")?,
        None => None,
    };
    match status.as_deref() {
        Some("sending") => {}
        Some("paused") => return Ok(Tried::Back),
        _ => return Ok(skipped("cancelled")),
    }
    let route = sql(
        &format!("{} AND c.id = $4", audience_sql()),
        vec![
            c.team.into(),
            c.tag.clone().into(),
            c.platform.clone().into(),
            c.contact.into(),
        ],
    );
    let Some(route) = db.query_one_raw(route).await? else {
        let there = sql(
            "SELECT 1 FROM contacts WHERE id = $1",
            vec![c.contact.into()],
        );
        let there = db.query_one_raw(there).await?.is_some();
        return Ok(skipped(if there { "untagged" } else { "contact_gone" }));
    };
    if let Some(reason) = route.try_get::<Option<String>>("", "reason")? {
        return Ok(skipped(&reason));
    }
    let item = route.try_get::<Option<Uuid>>("", "item_id")?;
    let item = match item {
        Some(id) => inbox_items::Entity::find_by_id(id).one(db).await?,
        None => None,
    };
    let account = match &item {
        Some(i) => {
            social_accounts::Entity::find_by_id(i.social_account_id)
                .one(db)
                .await?
        }
        None => None,
    };
    let (Some(item), Some(account)) = (item, account) else {
        return Ok(skipped("no_conversation"));
    };
    let name: Option<String> = route.try_get("", "display_name")?;
    let (text, filled) = automations::render(&c.text, name.as_deref());
    // Committed before the send starts: from here on the recipient counts as sent to, whatever
    // becomes of this process. Only one holder of a claim gets through.
    let begin = sql(
        "UPDATE broadcast_recipients SET status = 'delivering', updated_at = now() \
         WHERE id = $1 AND status = 'sending'",
        vec![c.id.into()],
    );
    if db.execute_raw(begin).await?.rows_affected() != 1 {
        return Ok(Tried::Lost);
    }
    // Before the send, so whatever becomes of it, no other broadcast reaches them today.
    let mark = sql(
        "UPDATE contacts SET last_broadcast_at = now() WHERE id = $1",
        vec![c.contact.into()],
    );
    db.execute_raw(mark).await?;
    let from = c.created_by;
    let sent = inbox::send(app, &account, &item, Via::Dm, &text, &filled, from, None).await;
    Ok(match sent {
        Ok(Ok(_)) => Tried::End("sent", None),
        // Found before any call, or the person takes no DMs: nothing went out, and it says
        // nothing about the next person.
        Ok(Err(
            e @ (InboxError::Invalid(_) | InboxError::DmNotAllowed | InboxError::NoDmAccess),
        )) => skipped(e.code()),
        // Anything else the platform says (a rate limit, a block, a policy or permission
        // error, an outage, a dead token) would meet the next recipient too: stop here.
        Ok(Err(e)) => {
            if matches!(e, InboxError::Reconnect | InboxError::MetaReconnect) {
                expiry::flag_reconnect(db, account.id).await;
            }
            Tried::Trip(e.code(), e.message())
        }
        // It may have gone out (the reply row is written after the send): never again.
        Err(e) => {
            tracing::error!(error = %e, recipient = %c.id, "broadcast send failed");
            Tried::End("failed", Some("internal".to_owned()))
        }
    })
}

/// `attempt`, then the recipient's end state; `true` when the broadcast was paused over it. A
/// skip gives its hit of the cap back.
async fn deliver(app: &AppState, c: &Claimed) -> Result<bool, DbErr> {
    let db = &app.db;
    let (status, reason, trip) = match attempt(app, c).await {
        Ok(Tried::Back) => return release(db, c).await.map(|()| false),
        Ok(Tried::Lost) => {
            daily_refund(db, &c.hits).await;
            return Ok(false);
        }
        Ok(Tried::End(status, reason)) => (status, reason, None),
        Ok(Tried::Trip(code, why)) => ("failed", Some(code.to_owned()), Some(why)),
        // Before the send: every query of `attempt` comes before it.
        Err(e) => {
            tracing::error!(error = %e, recipient = %c.id, "broadcast recipient check failed");
            ("failed", Some("internal".to_owned()), None)
        }
    };
    // First of all: the others must stop even if this recipient's own row cannot be written.
    if let Some(why) = &trip {
        pause(db, c.broadcast, why).await?;
    }
    let end = sql(
        "UPDATE broadcast_recipients SET status = $2, reason = $3, updated_at = now() \
         WHERE id = $1 AND status IN ('sending', 'delivering')",
        vec![c.id.into(), status.into(), reason.into()],
    );
    db.execute_raw(end).await?;
    if status == "skipped" {
        daily_refund(db, &c.hits).await;
    }
    Ok(trip.is_some() || (status == "failed" && pause_failing(db, Some(c.broadcast)).await?))
}

/// Hands back a recipient nothing was sent to (the round ran out of time, or the broadcast was
/// paused or cancelled): see `HAND_BACK`.
async fn release(db: &DatabaseConnection, c: &Claimed) -> Result<(), DbErr> {
    let back = format!("{HAND_BACK} AND r.id = $1");
    db.execute_raw(sql(&back, vec![c.id.into()])).await?;
    daily_refund(db, &c.hits).await;
    Ok(())
}

/// Sends to the claimed recipients: broadcasts side by side, one's recipients one after
/// another, until one of them pauses it; the rest are then handed back.
async fn work(app: &AppState, claimed: Vec<Claimed>, budget: Duration) {
    let started = Instant::now();
    let mut by_broadcast: HashMap<Uuid, Vec<Claimed>> = HashMap::new();
    for c in claimed {
        by_broadcast.entry(c.broadcast).or_default().push(c);
    }
    let runs: Vec<_> = by_broadcast
        .into_values()
        .map(|list| {
            let app = app.clone();
            tokio::spawn(async move {
                let mut paused = false;
                for c in &list {
                    let done = if paused || started.elapsed() >= budget {
                        release(&app.db, c).await
                    } else {
                        deliver(&app, c).await.map(|p| paused = p)
                    };
                    if let Err(e) = done {
                        tracing::error!(error = %e, recipient = %c.id, "broadcast recipient not settled");
                    }
                }
            })
        })
        .collect();
    for run in runs {
        let _ = run.await;
    }
}

/// A sending broadcast with nobody left to send to is `sent`; so is a paused one, which has
/// nothing to resume.
async fn finish(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    let stmt = "UPDATE broadcasts b SET status = 'sent', pause_reason = NULL, finished_at = now() \
                WHERE b.status IN ('sending', 'paused') AND NOT EXISTS (SELECT 1 \
                FROM broadcast_recipients r WHERE r.broadcast_id = b.id \
                AND r.status IN ('pending', 'sending', 'delivering'))";
    db.execute_raw(sql(stmt, vec![])).await.map(|_| ())
}

/// A round without the lock, with `budget` to start its sends; how many recipients it claimed.
pub async fn round(app: &AppState, budget: Duration) -> Result<usize, DbErr> {
    let txn = app.db.begin().await?;
    let claimed = claim(&txn).await?;
    txn.commit().await?;
    let n = claimed.len();
    work(app, claimed, budget).await;
    finish(&app.db).await?;
    Ok(n)
}

/// One round of broadcasts. Skips while another instance is claiming. The lock and its
/// transaction last for the recovery and the claim only, never for a send.
pub async fn tick(app: &AppState) -> Result<(), DbErr> {
    let lock = app.db.begin().await?;
    let row = lock
        .query_one_raw(sql(
            "SELECT pg_try_advisory_xact_lock($1) AS ok",
            vec![BROADCAST_LOCK.into()],
        ))
        .await?;
    let locked: bool = match row {
        Some(r) => r.try_get("", "ok")?,
        None => false,
    };
    if !locked {
        return Ok(());
    }
    recover(&lock).await?;
    // Recovered recipients are failures too: a broadcast that keeps being cut off stops.
    pause_failing(&lock, None).await?;
    let claimed = claim(&lock).await?;
    lock.commit().await?;
    work(app, claimed, ROUND).await;
    finish(&app.db).await
}
