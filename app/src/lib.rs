pub mod ai;
pub mod automation_effects;
pub mod automation_misses;
pub mod automations;
pub mod billing;
pub mod bluesky_actions;
pub mod broadcasts;
pub mod config;
pub mod connectors;
pub mod controllers;
pub mod dry_run;
pub mod expiry;
pub mod feeds;
pub mod inbox;
pub mod linkedin_inbox;
pub mod mail;
pub mod mcp;
pub mod media;
pub mod meta_inbox;
pub mod meta_rules;
pub mod metrics;
pub mod models;
pub mod newsletters;
pub mod post_mail;
pub mod publish;
pub mod rate_limit;
pub mod repeat;
pub mod scheduler;
pub mod state;
pub mod stripe;
pub mod threads_inbox;
pub mod turnstile;
pub mod webhooks;
pub mod x_automation;
pub mod x_inbox;
pub mod youtube_inbox;

use migration::{Migrator, MigratorTrait};
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbErr, TransactionTrait,
};
use std::time::Duration;

/// Any fixed number: every brain process asks Postgres for this same advisory lock.
const MIGRATION_LOCK: i64 = 0x736f_6369_616c; // "social"

/// Longest a request, or a tick that needs a second connection, waits for a free one.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

/// Connects to `DATABASE_URL` and applies pending migrations.
///
/// Two processes starting together would both try to create the migrations table, and one
/// would die with Postgres 42710. So one waits: the lock lives in a transaction on its own
/// connection, held while the migrations run on the pool, and Postgres frees it on commit or
/// if this process dies.
pub async fn connect(config: &config::Config) -> Result<DatabaseConnection, DbErr> {
    let mut options = ConnectOptions::new(&config.database_url);
    options
        .max_connections(config.database_max_connections)
        .connect_timeout(Duration::from_secs(10))
        .acquire_timeout(ACQUIRE_TIMEOUT);
    let db = Database::connect(options).await?;
    let lock = db.begin().await?;
    lock.execute_unprepared(&format!("SELECT pg_advisory_xact_lock({MIGRATION_LOCK})"))
        .await?;
    Migrator::up(&db, None).await?;
    lock.commit().await?;
    Ok(db)
}

/// `connect`, tried again every `retry` until it works or `wait` has passed: a Postgres that
/// restarts or fails over while brain starts costs a few seconds, not an outage that ends when
/// Railway gives up restarting the process. Whatever stops it for longer is returned.
pub async fn connect_when_ready(
    config: &config::Config,
    wait: Duration,
    retry: Duration,
) -> Result<DatabaseConnection, DbErr> {
    let give_up = tokio::time::Instant::now() + wait;
    loop {
        let e = match connect(config).await {
            Ok(db) => return Ok(db),
            // sea-orm puts the whole URL in some errors, and it carries the password.
            Err(e) => e.to_string().replace(&config.database_url, "DATABASE_URL"),
        };
        if tokio::time::Instant::now() + retry >= give_up {
            return Err(DbErr::Custom(e));
        }
        tracing::warn!(error = %e, "database not ready, trying again");
        tokio::time::sleep(retry).await;
    }
}
