//! Background jobs, in process: publishing due posts every minute (the old app ran every 5) and
//! purging deleted sets every hour, each run by one instance at a time through an advisory
//! lock. shortcut: no job queue; move to one if jobs need retries or exact times.
use std::time::Duration;

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement, TransactionTrait};

use crate::{publish, state::AppState};

/// How long a deleted social set can be restored before the purge removes it.
pub const RESTORE_DAYS: i64 = 30;

/// Advisory lock key held by the instance that purges.
const PURGE_LOCK: i64 = 0x0050_5552_4745_5345;

/// Starts the jobs; each first runs right away, so a restart also fails targets cut off
/// mid-publish.
pub fn spawn(app: AppState) {
    let db = app.db.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = publish::tick(&app).await {
                tracing::warn!(error = %e, "publish tick failed");
            }
        }
    });
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            match purge_deleted_sets(&db).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(sets = n, "purged deleted social sets"),
                Err(e) => tracing::warn!(error = %e, "social set purge failed"),
            }
        }
    });
}

/// Hard-deletes social sets deleted more than `RESTORE_DAYS` ago; their account links go by
/// cascade. Idempotent. Skips (returns 0) while another instance holds the lock.
pub async fn purge_deleted_sets(db: &DatabaseConnection) -> Result<u64, DbErr> {
    let txn = db.begin().await?;
    let lock = format!("SELECT pg_try_advisory_xact_lock({PURGE_LOCK}) AS ok");
    let row = txn
        .query_one_raw(Statement::from_string(DbBackend::Postgres, lock))
        .await?;
    let locked: bool = match row {
        Some(r) => r.try_get("", "ok")?,
        None => false,
    };
    if !locked {
        return Ok(0);
    }
    let sql = format!(
        "DELETE FROM social_sets WHERE deleted_at < now() - interval '{RESTORE_DAYS} days'"
    );
    let done = txn.execute_unprepared(&sql).await?;
    txn.commit().await?;
    Ok(done.rows_affected())
}
