//! Why an automation did not run on an item (`automation_misses`), kept so a person can answer
//! "why did this not fire?". A run that was held back for a safety reason (paused, cool-down,
//! caps, a connected account) is already a `skipped` row of `automation_runs`, with its reason;
//! this table holds the rest, one row per automation and item, with every reason that applied:
//! the codes of the conditions that failed (`automations::mismatches`, `button_not_named`),
//! `wrong_trigger` (the item is not what the automation listens for), `not_yet_on` (it was
//! switched on after the item came), `followed_before` (a new follower welcomed once already),
//! `no_public_page` and `stopped_by_other` (an earlier automation on the item had "stop the
//! others"). Rows go after 14 days, and past the newest 100 of each kind per automation.
use chrono::{DateTime, FixedOffset};
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement, Value};
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

/// How long a miss is kept.
pub const KEEP_DAYS: i32 = 14;
/// Most misses kept per automation, for items it listens for and for the others apart.
pub const KEEP_ROWS: i64 = 100;
/// Misses that only say the item was not this automation's to begin with.
const NOISE: &str = "reasons <@ ARRAY['wrong_trigger', 'wrong_account', 'wrong_set']";
/// Most misses an automation's page lists.
pub const LISTED: i64 = 50;

fn sql(sql: &str, values: Vec<Value>) -> Statement {
    Statement::from_sql_and_values(DbBackend::Postgres, sql, values)
}

/// Stores why automation `automation` did not run on `item`. Once per pair: a second pass over
/// the same item (the sweep) changes nothing.
pub async fn record(db: &impl ConnectionTrait, ids: (Uuid, Uuid, Uuid), reasons: &[&str]) {
    if let Err(e) = try_record(db, ids, reasons).await {
        tracing::warn!(error = %e, "automation miss not stored");
    }
}

async fn try_record(
    db: &impl ConnectionTrait,
    (automation, team, item): (Uuid, Uuid, Uuid),
    reasons: &[&str],
) -> Result<(), DbErr> {
    let reasons: Vec<String> = reasons.iter().map(|r| (*r).to_owned()).collect();
    let stmt = sql(
        "INSERT INTO automation_misses (automation_id, inbox_item_id, team_id, reasons) \
         VALUES ($1, $3, $2, $4) ON CONFLICT DO NOTHING",
        vec![automation.into(), team.into(), item.into(), reasons.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// For each active automation of the team that is not for this item's trigger, `wrong_trigger`;
/// for one that is but was switched on after the item came, `not_yet_on`.
pub async fn record_unreached(
    db: &impl ConnectionTrait,
    team: Uuid,
    item: Uuid,
    trigger: &str,
    received_at: DateTime<FixedOffset>,
) {
    if let Err(e) = try_unreached(db, team, item, trigger, received_at).await {
        tracing::warn!(error = %e, "automation misses not stored");
    }
}

async fn try_unreached(
    db: &impl ConnectionTrait,
    team: Uuid,
    item: Uuid,
    trigger: &str,
    received_at: DateTime<FixedOffset>,
) -> Result<(), DbErr> {
    let stmt = sql(
        "INSERT INTO automation_misses (automation_id, inbox_item_id, team_id, reasons) \
         SELECT a.id, $2, $1, ARRAY[CASE WHEN a.trigger <> $3 THEN 'wrong_trigger' \
         ELSE 'not_yet_on' END] FROM automations a WHERE a.team_id = $1 AND a.active \
         AND (a.trigger <> $3 OR a.activated_at > $4) ON CONFLICT DO NOTHING",
        vec![team.into(), item.into(), trigger.into(), received_at.into()],
    );
    db.execute_raw(stmt).await.map(|_| ())
}

/// Deletes misses older than `KEEP_DAYS` and those past the newest `KEEP_ROWS` of their kind. Rows
/// that only say the item was for another trigger, account or set (`NOISE`) are counted apart, so
/// they never push out a miss worth reading.
pub async fn prune(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    let old = sql(
        "DELETE FROM automation_misses WHERE created_at < now() - make_interval(days => $1)",
        vec![KEEP_DAYS.into()],
    );
    db.execute_raw(old).await?;
    let extra = sql(
        &format!("DELETE FROM automation_misses m USING ( \
           SELECT automation_id, inbox_item_id, row_number() OVER (PARTITION BY automation_id, {NOISE} \
           ORDER BY created_at DESC, inbox_item_id) AS n \
           FROM automation_misses) x \
         WHERE m.automation_id = x.automation_id AND m.inbox_item_id = x.inbox_item_id \
         AND x.n > $1"),
        vec![KEEP_ROWS.into()],
    );
    db.execute_raw(extra).await.map(|_| ())
}

/// One automation's verdict on an item, for the item's detail.
#[derive(Serialize, ToSchema, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub automation_id: Uuid,
    pub name: String,
    /// `ran` (a run that did something, or is still going), `failed` or `skipped` (a run held
    /// back or that went nowhere; `reasons` holds its reason) or `not_matched` (it never started;
    /// `reasons` holds the codes).
    pub outcome: String,
    /// Why not, as codes: a run's `reason`, or the codes of a miss. Empty for `ran`.
    pub reasons: Vec<String>,
}

/// What each of the team's automations did with an item: its run when it has one, else its miss.
/// Automations with neither (switched off, or created after) are left out.
pub async fn for_item(
    db: &impl ConnectionTrait,
    team: Uuid,
    item: Uuid,
) -> Result<Vec<Verdict>, DbErr> {
    let rows = db
        .query_all_raw(sql(
            "SELECT a.id, a.name, r.status AS run_status, r.reason AS run_reason, \
             m.reasons AS miss FROM automations a \
             LEFT JOIN automation_runs r ON r.automation_id = a.id AND r.inbox_item_id = $2 \
             LEFT JOIN automation_misses m ON m.automation_id = a.id AND m.inbox_item_id = $2 \
             WHERE a.team_id = $1 AND (r.id IS NOT NULL OR m.inbox_item_id IS NOT NULL) \
             ORDER BY a.created_at, a.id",
            vec![team.into(), item.into()],
        ))
        .await?;
    rows.iter()
        .map(|r| {
            let status: Option<String> = r.try_get("", "run_status")?;
            let reason: Option<String> = r.try_get("", "run_reason")?;
            let miss: Option<Vec<String>> = r.try_get("", "miss")?;
            let (outcome, reasons) = match (status.as_deref(), miss) {
                (Some(s @ ("skipped" | "failed")), _) => (s, reason.into_iter().collect()),
                (Some(_), _) => ("ran", Vec::new()),
                (None, miss) => ("not_matched", miss.unwrap_or_default()),
            };
            Ok(Verdict {
                automation_id: r.try_get("", "id")?,
                name: r.try_get("", "name")?,
                outcome: outcome.into(),
                reasons,
            })
        })
        .collect()
}

/// A recent item an automation did not match, for the automation's page.
#[derive(Serialize, ToSchema, Debug)]
pub struct Miss {
    pub inbox_item_id: Uuid,
    pub reasons: Vec<String>,
    pub created_at: DateTime<FixedOffset>,
    /// `comment`, `mention`, `dm`, `follow`, `like` or `repost`.
    pub kind: String,
    pub platform: String,
    /// The author's display name or handle.
    pub author: Option<String>,
    /// The item's text, cut to 200 characters.
    pub text: String,
}

/// An automation's newest misses for items it listens for (the `NOISE` ones are left out: they
/// would be every item of another trigger, account or set), newest first.
pub async fn for_automation(
    db: &impl ConnectionTrait,
    team: Uuid,
    automation: Uuid,
) -> Result<Vec<Miss>, DbErr> {
    let rows = db
        .query_all_raw(sql(
            &format!(
                "SELECT m.inbox_item_id, m.reasons, m.created_at, i.kind, i.platform, \
             COALESCE(i.author_name, i.author_handle) AS author, left(i.text, 200) AS text \
             FROM automation_misses m JOIN inbox_items i ON i.id = m.inbox_item_id \
             WHERE m.team_id = $1 AND m.automation_id = $2 AND NOT m.{NOISE} \
             ORDER BY m.created_at DESC, m.inbox_item_id LIMIT $3"
            ),
            vec![team.into(), automation.into(), LISTED.into()],
        ))
        .await?;
    rows.iter()
        .map(|r| {
            Ok(Miss {
                inbox_item_id: r.try_get("", "inbox_item_id")?,
                reasons: r.try_get("", "reasons")?,
                created_at: r.try_get("", "created_at")?,
                kind: r.try_get("", "kind")?,
                platform: r.try_get("", "platform")?,
                author: r.try_get("", "author")?,
                text: r.try_get("", "text")?,
            })
        })
        .collect()
}
