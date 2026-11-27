//! Gives back X credits that were spent for a post that never went out. A target holds its
//! credits before sending (`publish_target_held`, ledger reason `x post`, reference
//! `{target}:{uuid}`) and settles them when its attempt ends (`release_x`): the unsent ones are
//! granted back under the same reference, and when all stay spent the spend's reason becomes
//! `x post kept` (`KEPT`). When that settling is lost (a database error, a crash between the
//! target failing and `release_x`) the team stays charged. This hourly sweep finds such a spend:
//! reason still `x post`, no grant under its reference, older than `AFTER_MINUTES` (a publish
//! ends well before: `publish::PUBLISH_LIMIT`), the latest spend of its
//! target and untouched since its attempt ended (so the target's error is that attempt's), whose target ended `failed` with no remote
//! id, no thread part out and an error that is certainly unsent (`publish::credit_kept` says
//! no). It grants the credits back under the spend's own reference, so a second run, or a second
//! instance, grants nothing more (the ledger's unique index), reason `x post unsent (swept)`.
//! Does nothing without `BILLING_ENABLED`.
//! shortcut: spends made before `KEPT` existed have no mark and are judged like the rest (billing
//! was never on in production when this was written, so there are none there).
//! shortcut: a user's Retry within `ENDED_WITHIN_MINUTES` of an attempt that kept its credit
//! (a dropped connection) whose `x post kept` mark was lost, followed by an attempt that fails
//! before spending, still looks like a lost release; it takes three coincidences.
//! shortcut: a crash in the middle of a send leaves the target failed as "may have posted"
//! (`recover_stuck`), which is not told from a crash before the first call, so its credits
//! stay spent; a thread that failed with parts out keeps them too. Settle those by hand with
//! `POST /v1/admin/teams/{id}/credits` after looking at the account.
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};
use uuid::Uuid;

use crate::{billing, publish, state::AppState};

/// The reason of a spend whose credits all stayed spent, settled by `release_x`.
pub const KEPT: &str = "x post kept";
/// How old a spend must be before it is judged: no publish is still running.
const AFTER_MINUTES: i32 = 120;
/// The target must have last changed this soon after the spend: it then still holds the error of
/// the attempt that spent (a publish ends within `publish::PUBLISH_LIMIT`, 14 minutes). A target
/// touched later was retried or changed, and its error may be another attempt's.
const ENDED_WITHIN_MINUTES: i32 = 20;
/// How far back to look: older leftovers are not worth a scan every hour.
const WITHIN_DAYS: i32 = 7;
/// Most spends judged in one run.
const BATCH: i64 = 200;

/// Credits given back and posts they were for in this run.
pub async fn sweep(app: &AppState) -> Result<(i64, u64), DbErr> {
    if !app.billing_enabled {
        return Ok((0, 0));
    }
    let rows = app
        .db
        .query_all_raw(Statement::from_sql_and_values(
            DbBackend::Postgres,
            // The guard on `ref` comes before the cast, so a reference of another shape never
            // reaches it.
            "SELECT l.team_id, l.ref, -l.amount AS amount, t.error \
             FROM credit_ledger l JOIN post_targets t ON t.id = (CASE WHEN l.ref ~ \
             '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}:' \
             THEN left(l.ref, 36)::uuid END) \
             WHERE l.kind = 'x' AND l.amount < 0 AND l.reason = 'x post' \
             AND l.created_at < now() - make_interval(mins => $1) \
             AND l.created_at > now() - make_interval(days => $2) \
             AND NOT EXISTS (SELECT 1 FROM credit_ledger n WHERE n.team_id = l.team_id \
             AND n.kind = 'x' AND n.amount < 0 AND n.reason IN ('x post', 'x post kept') \
             AND left(n.ref, 37) = left(l.ref, 37) AND n.created_at > l.created_at) \
             AND t.updated_at BETWEEN l.created_at AND l.created_at + make_interval(mins => $5) \
             AND t.status = 'failed' AND t.remote_id IS NULL AND t.parts = '[]'::jsonb \
             AND t.error IS NOT NULL AND t.error NOT LIKE '%may have posted%' \
             AND t.error NOT LIKE '%may still post%' AND t.error NOT LIKE '%may be live%' \
             AND (t.error NOT LIKE '%could not be reached%' OR t.error LIKE '%' || $4 || '%') \
             AND NOT EXISTS (SELECT 1 FROM credit_ledger g WHERE g.team_id = l.team_id \
             AND g.kind = 'x' AND g.ref = l.ref AND g.amount > 0) \
             ORDER BY l.created_at LIMIT $3",
            vec![
                AFTER_MINUTES.into(),
                WITHIN_DAYS.into(),
                BATCH.into(),
                crate::connectors::UNSENT.into(),
                ENDED_WITHIN_MINUTES.into(),
            ],
        ))
        .await?;
    let (mut credits, mut posts) = (0, 0);
    for row in rows {
        let error: Option<String> = row.try_get("", "error")?;
        // The query leaves out what says it may have posted; this is the same rule once more.
        let Some(error) = error else { continue };
        if publish::credit_kept(&error) {
            continue;
        }
        let team: Uuid = row.try_get("", "team_id")?;
        let reference: String = row.try_get("", "ref")?;
        let amount: i64 = row.try_get("", "amount")?;
        let kind = billing::Kind::X;
        let reason = "x post unsent (swept)";
        if billing::grant(&app.db, team, kind, amount, reason, Some(&reference)).await? {
            credits += amount;
            posts += 1;
        }
    }
    Ok((credits, posts))
}
