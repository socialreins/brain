//! Gives back X credits that were spent for a post that never went out. A target holds its
//! credits before sending (`publish_target_held`, ledger reason `x post`, reference
//! `{target}:{uuid}`) and gives back the unsent ones when it ends (`release_x`); when that
//! release is lost (a database error, a crash between the target failing and the release) the
//! team stays charged. This hourly sweep finds such a spend: no matching grant under the same
//! reference, older than `AFTER` (a publish ends well before: `publish::PUBLISH_LIMIT`), whose
//! target ended `failed` with no remote id, no thread part out and an error that is certainly
//! unsent (`publish::credit_kept` says no). It grants the credits back under the spend's own
//! reference, so a second run, or a second instance, grants nothing more (the ledger's unique
//! index), reason `x post unsent (swept)`. Does nothing without `BILLING_ENABLED`.
//! shortcut: a crash in the middle of a send leaves the target failed as "may have posted"
//! (`recover_stuck`), which is not told from a crash before the first call, so its credits
//! stay spent; a thread that failed with parts out keeps them too. Settle those by hand with
//! `POST /v1/admin/teams/{id}/credits` after looking at the account.
use sea_orm::{ConnectionTrait, DbBackend, DbErr, Statement};
use uuid::Uuid;

use crate::{billing, publish, state::AppState};

/// How old a spend must be before it is judged: no publish is still running.
const AFTER_MINUTES: i32 = 120;
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
             AND t.status = 'failed' AND t.remote_id IS NULL AND t.parts = '[]'::jsonb \
             AND NOT EXISTS (SELECT 1 FROM credit_ledger g WHERE g.team_id = l.team_id \
             AND g.kind = 'x' AND g.ref = l.ref AND g.amount > 0) \
             ORDER BY l.created_at LIMIT $3",
            vec![AFTER_MINUTES.into(), WITHIN_DAYS.into(), BATCH.into()],
        ))
        .await?;
    let (mut credits, mut posts) = (0, 0);
    for row in rows {
        let error: Option<String> = row.try_get("", "error")?;
        // No reason stored is not a reason to say it sent nothing.
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
