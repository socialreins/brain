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

/// Starts the jobs; each first runs right away, so a restart also fails targets cut off
/// mid-publish.
pub fn spawn(app: AppState) {
    let db = app.db.clone();
    let inbox_app = app.clone();
    let hook_app = app.clone();
    let sweep_app = app.clone();
    let threads_app = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(24 * 3600));
        loop {
            tick.tick().await;
            match threads::refresh_tokens(&threads_app).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(accounts = n, "extended threads tokens"),
                Err(e) => tracing::warn!(error = %e, "threads token refresh failed"),
            }
            match linkedin::refresh_tokens(&threads_app).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(accounts = n, "renewed linkedin tokens"),
                Err(e) => tracing::warn!(error = %e, "linkedin token refresh failed"),
            }
            match pinterest::refresh_tokens(&threads_app).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(accounts = n, "renewed pinterest tokens"),
                Err(e) => tracing::warn!(error = %e, "pinterest token refresh failed"),
            }
            match expiry::notify(&threads_app).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(accounts = n, "mailed about expiring or refused tokens"),
                Err(e) => tracing::warn!(error = %e, "expiry mail failed"),
            }
        }
    });
    let x_app = app.clone();
    tokio::spawn(async move {
        // X access tokens last two hours.
        let mut tick = tokio::time::interval(Duration::from_secs(15 * 60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            match x::refresh_tokens(&x_app).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(accounts = n, "renewed x tokens"),
                Err(e) => tracing::warn!(error = %e, "x token refresh failed"),
            }
        }
    });
    let feed_app = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(feeds::EVERY_SECS as u64));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = feeds::tick(&feed_app).await {
                tracing::warn!(error = %e, "feed tick failed");
            }
        }
    });
    let metrics_app = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(metrics::EVERY_SECS));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = metrics::tick(&metrics_app).await {
                tracing::warn!(error = %e, "metrics tick failed");
            }
        }
    });
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = webhooks::tick(&hook_app).await {
                tracing::warn!(error = %e, "webhook tick failed");
            }
        }
    });
    let flow_app = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = automations::tick(&flow_app).await {
                tracing::warn!(error = %e, "automation flow tick failed");
            }
        }
    });
    let broadcast_app = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = broadcasts::tick(&broadcast_app).await {
                tracing::warn!(error = %e, "broadcast tick failed");
            }
        }
    });
    let newsletter_app = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = newsletters::tick(&newsletter_app).await {
                tracing::warn!(error = %e, "newsletter tick failed");
            }
        }
    });
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = inbox::tick(&inbox_app).await {
                tracing::warn!(error = %e, "inbox tick failed");
            }
        }
    });
    let x_app = app.clone();
    tokio::spawn(async move {
        // Which X accounts are due, and how often, is the account's own setting (`x_inbox`).
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = crate::x_inbox::tick(&x_app).await {
                tracing::warn!(error = %e, "x inbox tick failed");
            }
        }
    });
    let threads_app = app.clone();
    tokio::spawn(async move {
        // Which Threads accounts are due (every 5 minutes, or at once after a webhook hint) is
        // decided by the account's own stamps (`threads_inbox`).
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = crate::threads_inbox::tick(&threads_app).await {
                tracing::warn!(error = %e, "threads inbox tick failed");
            }
        }
    });
    let youtube_app = app.clone();
    tokio::spawn(async move {
        // Which channels are due, and how often, is the channel's own setting (`youtube_inbox`).
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = crate::youtube_inbox::tick(&youtube_app).await {
                tracing::warn!(error = %e, "youtube inbox tick failed");
            }
        }
    });
    let linkedin_app = app.clone();
    tokio::spawn(async move {
        // Does nothing until LinkedIn approved the Community Management API (`linkedin_inbox`).
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = crate::linkedin_inbox::tick(&linkedin_app).await {
                tracing::warn!(error = %e, "linkedin inbox tick failed");
            }
        }
    });
    let meta_app = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = meta_inbox::tick(&meta_app).await {
                tracing::warn!(error = %e, "meta backfill tick failed");
            }
        }
    });
    tokio::spawn(async move {
        // Posts a killed process claimed and never started go out at the first round.
        match publish::release_unstarted(&app.db).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(posts = n, "released posts claimed and not started"),
            Err(e) => tracing::warn!(error = %e, "releasing unstarted posts failed"),
        }
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            // Not waited for: a post that takes many minutes must not delay the next round.
            // Run as its own task: a panic in a round is logged and the next tick still comes,
            // instead of ending this loop and leaving every scheduled post unpublished.
            let round = app.clone();
            match tokio::spawn(async move { publish::begin(&round).await }).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => tracing::warn!(error = %e, "publish tick failed"),
                Err(e) => tracing::error!(error = %e, "publish tick panicked"),
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
            match purge_idempotency_keys(&db).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(keys = n, "purged old idempotency keys"),
                Err(e) => tracing::warn!(error = %e, "idempotency key purge failed"),
            }
            match purge(&db, &webhooks::purge_sql()).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(deliveries = n, "purged old webhook deliveries"),
                Err(e) => tracing::warn!(error = %e, "webhook delivery purge failed"),
            }
            match media::sweep(&sweep_app).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(files = n, "deleted stored media"),
                Err(e) => tracing::warn!(error = %e, "stored media sweep failed"),
            }
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
