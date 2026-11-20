//! Broadcasts: one DM to every reachable contact with a tag. A start (`start`) snapshots the
//! audience into `broadcast_recipients`; `tick` (every minute, own advisory lock) then claims up
//! to 20 pending recipients of each sending broadcast in one short transaction (`claim`, with
//! each send's hit of the daily cap), commits, and only then sends through `inbox::send`.
//! A recipient moves `pending` to `sending` to an end state and never back once a send was
//! started, so nobody is sent a broadcast twice: one cut off mid-send ends `failed: interrupted`
//! (`recover`). Never send to a recipient without claiming it, and never retry one.
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
// it shows in the inbox conversation.
const ROUTE: &str = "SELECT c.id AS contact_id, c.display_name, c.platform, \
    CASE WHEN c.opted_out THEN 'opted_out' \
         WHEN l.id IS NULL THEN 'no_conversation' \
         WHEN a.id IS NULL OR COALESCE(a.meta->>'reconnect', 'false') = 'true' \
              OR (c.platform = 'bluesky' AND NOT a.dm_access) THEN 'account_disconnected' \
         WHEN c.platform <> 'bluesky' AND l.received_at <= now() - interval '24 hours' \
              THEN 'outside_window' END AS reason, \
    COALESCE(d.id, l.id) AS item_id \
    FROM contacts c \
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

/// Cancels a `sending` broadcast: its pending recipients are `skipped: cancelled`. `None` when
/// the team has no such broadcast, `false` when it is not sending. A recipient already claimed
/// ends on its own: `deliver` looks at the broadcast again before it sends.
pub async fn cancel(db: &DatabaseConnection, team: Uuid, id: Uuid) -> Result<Option<bool>, DbErr> {
    let txn = db.begin().await?;
    let key: Vec<Value> = vec![id.into(), team.into()];
    let stop = sql(
        "UPDATE broadcasts SET status = 'cancelled', finished_at = now() \
         WHERE id = $1 AND team_id = $2 AND status = 'sending'",
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

/// A claimed recipient: `sending`, with its hit of the daily cap taken.
struct Claimed {
    id: Uuid,
    broadcast: Uuid,
    contact: Option<Uuid>,
    text: String,
    created_by: Option<Uuid>,
    hits: Vec<i64>,
}

/// Recipients cut off mid-send fail as `interrupted`; they are never sent again.
async fn recover(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    let stmt = sql(
        "UPDATE broadcast_recipients SET status = 'failed', reason = 'interrupted', \
         updated_at = now() WHERE status = 'sending' \
         AND updated_at < now() - make_interval(mins => $1)",
        vec![STUCK_MINUTES.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// Moves up to `BATCH` pending recipients of every sending broadcast to `sending`, each with
/// its hit of the team's daily cap. Call it in a transaction that commits before any send:
/// `SKIP LOCKED` keeps two callers off the same recipient. At the cap the rest stay `pending`,
/// for a later round.
async fn claim(txn: &impl ConnectionTrait) -> Result<Vec<Claimed>, DbErr> {
    let sending = "SELECT id, team_id, text, created_by FROM broadcasts \
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
                contact: r.try_get("", "contact_id")?,
                text: b.try_get("", "text")?,
                created_by: b.try_get("", "created_by")?,
                hits,
            });
        }
    }
    Ok(out)
}

/// How a claimed recipient ended: the status, and why when it was not sent.
type Outcome = (&'static str, Option<String>);

fn skipped(reason: &str) -> Outcome {
    ("skipped", Some(reason.to_owned()))
}

/// Sends to one claimed recipient, once. Everything a start checked is checked again first:
/// the broadcast still sends, the contact is there and did not opt out, the account is
/// connected, and (Facebook, Instagram) the 24 hours still hold.
async fn attempt(app: &AppState, c: &Claimed) -> Result<Outcome, DbErr> {
    let db = &app.db;
    let live = sql(
        "SELECT 1 FROM broadcasts WHERE id = $1 AND status = 'sending'",
        vec![c.broadcast.into()],
    );
    if db.query_one_raw(live).await?.is_none() {
        return Ok(skipped("cancelled"));
    }
    let route = sql(&format!("{ROUTE} AND c.id = $1"), vec![c.contact.into()]);
    let Some(route) = db.query_one_raw(route).await? else {
        return Ok(skipped("contact_gone"));
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
    let via = Via::Dm;
    let sent = inbox::send(
        app,
        &account,
        &item,
        via,
        &text,
        &filled,
        c.created_by,
        None,
    )
    .await;
    Ok(match sent {
        Ok(Ok(_)) => ("sent", None),
        // Found before any call, or the person takes no DMs: nothing went out.
        Ok(Err(
            e @ (InboxError::Invalid(_) | InboxError::DmNotAllowed | InboxError::NoDmAccess),
        )) => skipped(e.code()),
        Ok(Err(e)) => {
            // A dead token: the rest of the audience through this account is then skipped
            // (`account_disconnected`) without a call each.
            if matches!(e, InboxError::Reconnect | InboxError::MetaReconnect) {
                expiry::flag_reconnect(db, account.id).await;
            }
            ("failed", Some(e.code().to_owned()))
        }
        // It may have gone out (the reply row is written after the send): never again.
        Err(e) => {
            tracing::error!(error = %e, recipient = %c.id, "broadcast send failed");
            ("failed", Some("internal".to_owned()))
        }
    })
}

/// `attempt`, then the recipient's end state. A skip gives its hit of the cap back.
async fn deliver(app: &AppState, c: &Claimed) -> Result<(), DbErr> {
    let (status, reason) = match attempt(app, c).await {
        Ok(o) => o,
        // Before the send: every query of `attempt` comes before it.
        Err(e) => {
            tracing::error!(error = %e, recipient = %c.id, "broadcast recipient check failed");
            ("failed", Some("internal".to_owned()))
        }
    };
    let end = sql(
        "UPDATE broadcast_recipients SET status = $2, reason = $3, updated_at = now() \
         WHERE id = $1 AND status = 'sending'",
        vec![c.id.into(), status.into(), reason.into()],
    );
    app.db.execute_raw(end).await?;
    if status == "skipped" {
        daily_refund(&app.db, &c.hits).await;
    }
    Ok(())
}

/// Hands back a recipient the round had no time for: nothing was sent, so it is `pending` again.
async fn release(db: &DatabaseConnection, c: &Claimed) -> Result<(), DbErr> {
    let back = sql(
        "UPDATE broadcast_recipients SET status = 'pending', updated_at = now() \
         WHERE id = $1 AND status = 'sending'",
        vec![c.id.into()],
    );
    db.execute_raw(back).await?;
    daily_refund(db, &c.hits).await;
    Ok(())
}

/// Sends to the claimed recipients: broadcasts side by side, one's recipients one after another.
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
                for c in &list {
                    let done = if started.elapsed() < budget {
                        deliver(&app, c).await
                    } else {
                        release(&app.db, c).await
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

/// A sending broadcast with nobody left to send to is `sent`.
async fn finish(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    let stmt = "UPDATE broadcasts b SET status = 'sent', finished_at = now() \
                WHERE b.status = 'sending' AND NOT EXISTS (SELECT 1 FROM broadcast_recipients r \
                WHERE r.broadcast_id = b.id AND r.status IN ('pending', 'sending'))";
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
    let claimed = claim(&lock).await?;
    lock.commit().await?;
    work(app, claimed, ROUND).await;
    finish(&app.db).await
}
