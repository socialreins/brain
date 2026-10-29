//! Background jobs. shortcut: an hourly in-process tick, run by one instance at a time through
//! an advisory lock; move to a real scheduler once there are jobs that need exact times.
use std::time::Duration;

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement, TransactionTrait};

/// How long a deleted social set can be restored before the purge removes it.
pub const RESTORE_DAYS: i64 = 30;

/// Advisory lock key held by the instance that purges.
const PURGE_LOCK: i64 = 0x0050_5552_4745_5345;

/// Starts the hourly jobs; the first run is right away.
pub fn spawn(db: DatabaseConnection) {
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
