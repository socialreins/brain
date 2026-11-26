//! Background jobs, in process: publishing due posts and reading inboxes (then running
//! automations), taking up waiting automation flows, sending broadcasts, sending newsletters and sending webhooks every minute (the old app ran every 5), and purging deleted
//! sets, old idempotency keys, old webhook deliveries and leftover stored media every hour, and
//! extending Threads and LinkedIn tokens and mailing about ending ones daily and renewing X tokens, reading RSS feeds and reading post metrics every 15 minutes, each run by one instance at a time through an advisory lock.
//! shortcut: no job queue; move to one if jobs need retries or exact times.
use std::time::Duration;

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement, TransactionTrait};

use crate::{
    automations, broadcasts,
    controllers::{idempotency, linkedin, pinterest, threads, x},
    expiry, feeds, inbox, media, meta_inbox, metrics, newsletters, publish,
    state::AppState,
    webhooks,
};

/// How long a deleted social set can be restored before the purge removes it.
pub const RESTORE_DAYS: i64 = 30;

/// Advisory lock key held by the instance that purges.
const PURGE_LOCK: i64 = 0x0050_5552_4745_5345;

/// The longest wait after panics in a row.
const BACKOFF_MAX: Duration = Duration::from_secs(300);

/// How long to wait after the `panics`-th panic in a row: one period, doubling, up to
/// `BACKOFF_MAX` (the next tick still waits out its own period). A round that always panics
/// is retried slower and slower, never in a hot loop.
fn backoff(period: Duration, panics: u32) -> Duration {
    period
        .saturating_mul(2u32.saturating_pow(panics.saturating_sub(1)))
        .min(BACKOFF_MAX)
}

/// Runs `round(state)` now and then every `period` (a late round delays the next), each round
/// as its own task: a panic is logged as an error with the loop's `name` and the loop waits a
/// bounded back-off and carries on, instead of ending for good with `/health` still ok.
/// Dropping the returned future (the runtime stopping) aborts the round with it. Rounds are
/// not waited for beyond their own return: a round that starts longer work spawns it itself.
async fn every<S, F, Fut>(name: &'static str, period: Duration, state: S, round: F)
where
    S: Clone,
    F: Fn(S) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut panics: u32 = 0;
    loop {
        tick.tick().await;
        // A JoinSet, not `tokio::spawn`: dropping it aborts the round.
        let mut run = tokio::task::JoinSet::new();
        run.spawn(round(state.clone()));
        match run.join_next().await {
            Some(Err(e)) => {
                panics = panics.saturating_add(1);
                tracing::error!(job = name, error = %e, consecutive_panics = panics, "background round panicked");
                tokio::time::sleep(backoff(period, panics)).await;
            }
            _ => panics = 0,
        }
    }
}

fn start<F, Fut>(name: &'static str, period: Duration, app: &AppState, round: F)
where
    F: Fn(AppState) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(every(name, period, app.clone(), round));
}

/// Starts the jobs; each first runs right away, so a restart also fails targets cut off
/// mid-publish. Each runs under `every`, so a panic in a round does not end its job.
pub fn spawn(app: AppState) {
    let min = Duration::from_secs(60);
    start(
        "tokens",
        Duration::from_secs(24 * 3600),
        &app,
        |app| async move {
            match threads::refresh_tokens(&app).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(accounts = n, "extended threads tokens"),
                Err(e) => tracing::warn!(error = %e, "threads token refresh failed"),
            }
            match linkedin::refresh_tokens(&app).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(accounts = n, "renewed linkedin tokens"),
                Err(e) => tracing::warn!(error = %e, "linkedin token refresh failed"),
            }
            match pinterest::refresh_tokens(&app).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(accounts = n, "renewed pinterest tokens"),
                Err(e) => tracing::warn!(error = %e, "pinterest token refresh failed"),
            }
            match expiry::notify(&app).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(accounts = n, "mailed about expiring or refused tokens"),
                Err(e) => tracing::warn!(error = %e, "expiry mail failed"),
            }
        },
    );
    // X access tokens last two hours.
    start(
        "x tokens",
        Duration::from_secs(15 * 60),
        &app,
        |app| async move {
            match x::refresh_tokens(&app).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(accounts = n, "renewed x tokens"),
                Err(e) => tracing::warn!(error = %e, "x token refresh failed"),
            }
        },
    );
    start(
        "feeds",
        Duration::from_secs(feeds::EVERY_SECS as u64),
        &app,
        |app| async move {
            if let Err(e) = feeds::tick(&app).await {
                tracing::warn!(error = %e, "feed tick failed");
            }
        },
    );
    start(
        "metrics",
        Duration::from_secs(metrics::EVERY_SECS),
        &app,
        |app| async move {
            if let Err(e) = metrics::tick(&app).await {
                tracing::warn!(error = %e, "metrics tick failed");
            }
        },
    );
    start("webhooks", min, &app, |app| async move {
        if let Err(e) = webhooks::tick(&app).await {
            tracing::warn!(error = %e, "webhook tick failed");
        }
    });
    start("automation flows", min, &app, |app| async move {
        if let Err(e) = automations::tick(&app).await {
            tracing::warn!(error = %e, "automation flow tick failed");
        }
    });
    start("broadcasts", min, &app, |app| async move {
        if let Err(e) = broadcasts::tick(&app).await {
            tracing::warn!(error = %e, "broadcast tick failed");
        }
    });
    start("newsletters", min, &app, |app| async move {
        if let Err(e) = newsletters::tick(&app).await {
            tracing::warn!(error = %e, "newsletter tick failed");
        }
    });
    start("inbox", min, &app, |app| async move {
        if let Err(e) = inbox::tick(&app).await {
            tracing::warn!(error = %e, "inbox tick failed");
        }
    });
    // Which X accounts are due, and how often, is the account's own setting (`x_inbox`).
    start("x inbox", min, &app, |app| async move {
        if let Err(e) = crate::x_inbox::tick(&app).await {
            tracing::warn!(error = %e, "x inbox tick failed");
        }
    });
    // Which Threads accounts are due (every 5 minutes, or at once after a webhook hint) is
    // decided by the account's own stamps (`threads_inbox`).
    start("threads inbox", min, &app, |app| async move {
        if let Err(e) = crate::threads_inbox::tick(&app).await {
            tracing::warn!(error = %e, "threads inbox tick failed");
        }
    });
    // Which channels are due, and how often, is the channel's own setting (`youtube_inbox`).
    start("youtube inbox", min, &app, |app| async move {
        if let Err(e) = crate::youtube_inbox::tick(&app).await {
            tracing::warn!(error = %e, "youtube inbox tick failed");
        }
    });
    // Does nothing until LinkedIn approved the Community Management API (`linkedin_inbox`).
    start("linkedin inbox", min, &app, |app| async move {
        if let Err(e) = crate::linkedin_inbox::tick(&app).await {
            tracing::warn!(error = %e, "linkedin inbox tick failed");
        }
    });
    start("meta backfill", min, &app, |app| async move {
        if let Err(e) = meta_inbox::tick(&app).await {
            tracing::warn!(error = %e, "meta backfill tick failed");
        }
    });
    let publish_app = app.clone();
    tokio::spawn(async move {
        // Posts a killed process claimed and never started go out at the first round. Its own
        // task: a panic here must not keep the publish loop from starting.
        let release = publish_app.clone();
        let released = tokio::spawn(async move { publish::release_unstarted(&release.db).await });
        match released.await {
            Ok(Ok(0)) => {}
            Ok(Ok(n)) => tracing::info!(posts = n, "released posts claimed and not started"),
            Ok(Err(e)) => tracing::warn!(error = %e, "releasing unstarted posts failed"),
            Err(e) => tracing::error!(error = %e, "releasing unstarted posts panicked"),
        }
        every("publish", min, publish_app, |app| async move {
            // Before `begin`, so the round already judges lateness without our own downtime.
            if let Err(e) = publish::note_tick(&app.db).await {
                tracing::warn!(error = %e, "publish tick not recorded");
            }
            // `begin` starts the posts as tasks and returns: a post that takes many minutes
            // does not delay the next round.
            if let Err(e) = publish::begin(&app).await {
                tracing::warn!(error = %e, "publish tick failed");
            }
        })
        .await
    });
    start("purge", Duration::from_secs(3600), &app, |app| async move {
        match purge_deleted_sets(&app.db).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(sets = n, "purged deleted social sets"),
            Err(e) => tracing::warn!(error = %e, "social set purge failed"),
        }
        match purge_idempotency_keys(&app.db).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(keys = n, "purged old idempotency keys"),
            Err(e) => tracing::warn!(error = %e, "idempotency key purge failed"),
        }
        match purge(&app.db, &webhooks::purge_sql()).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(deliveries = n, "purged old webhook deliveries"),
            Err(e) => tracing::warn!(error = %e, "webhook delivery purge failed"),
        }
        match media::sweep(&app).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(files = n, "deleted stored media"),
            Err(e) => tracing::warn!(error = %e, "stored media sweep failed"),
        }
    });
}

/// Hard-deletes social sets deleted more than `RESTORE_DAYS` ago; their account links go by
/// cascade. Idempotent. Skips (returns 0) while another instance holds the lock.
pub async fn purge_deleted_sets(db: &DatabaseConnection) -> Result<u64, DbErr> {
    let sql = format!(
        "DELETE FROM social_sets WHERE deleted_at < now() - interval '{RESTORE_DAYS} days'"
    );
    purge(db, &sql).await
}

/// Deletes idempotency keys past their 30 days, like `purge_deleted_sets`.
pub async fn purge_idempotency_keys(db: &DatabaseConnection) -> Result<u64, DbErr> {
    purge(db, &idempotency::purge_sql()).await
}

/// Runs `sql` under the purge lock; 0 while another instance holds it.
async fn purge(db: &DatabaseConnection, sql: &str) -> Result<u64, DbErr> {
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
    let done = txn.execute_unprepared(sql).await?;
    txn.commit().await?;
    Ok(done.rows_affected())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicU32, Ordering::SeqCst},
        Arc,
    };

    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[tokio::test]
    async fn a_panicking_round_does_not_end_the_loop() {
        let rounds = Arc::new(AtomicU32::new(0));
        let job = tokio::spawn(every("test", 5 * MS, rounds.clone(), |rounds| async move {
            // The first round panics, the ones after it count.
            if rounds.fetch_add(1, SeqCst) == 0 {
                panic!("boom");
            }
        }));
        tokio::time::sleep(300 * MS).await;
        assert!(!job.is_finished(), "the loop ended");
        assert!(
            rounds.load(SeqCst) >= 3,
            "rounds after the panic did not run"
        );
        job.abort();
    }

    #[tokio::test]
    async fn stopping_the_loop_aborts_the_round() {
        struct Dropped(Arc<AtomicU32>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.fetch_add(1, SeqCst);
            }
        }
        let started = Arc::new(AtomicU32::new(0));
        let dropped = Arc::new(AtomicU32::new(0));
        let job = tokio::spawn(every(
            "test",
            5 * MS,
            (started.clone(), dropped.clone()),
            |(started, dropped)| async move {
                let _guard = Dropped(dropped);
                started.fetch_add(1, SeqCst);
                tokio::time::sleep(Duration::from_secs(3600)).await;
            },
        ));
        tokio::time::sleep(50 * MS).await;
        assert_eq!(started.load(SeqCst), 1);
        job.abort();
        let _ = job.await;
        tokio::time::sleep(50 * MS).await;
        assert_eq!(dropped.load(SeqCst), 1, "the round outlived the loop");
        assert_eq!(started.load(SeqCst), 1, "a round started after the stop");
    }

    #[test]
    fn the_wait_after_panics_doubles_and_is_bounded() {
        let min = Duration::from_secs(60);
        assert_eq!(backoff(min, 1), min);
        assert_eq!(backoff(min, 2), 2 * min);
        assert_eq!(backoff(min, 3), 4 * min);
        assert_eq!(backoff(min, 4), BACKOFF_MAX);
        assert_eq!(backoff(min, u32::MAX), BACKOFF_MAX);
        assert_eq!(backoff(Duration::from_secs(24 * 3600), 1), BACKOFF_MAX);
    }
}
