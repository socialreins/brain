pub mod automations;
pub mod config;
pub mod connectors;
pub mod controllers;
pub mod expiry;
pub mod inbox;
pub mod mail;
pub mod mcp;
pub mod media;
pub mod meta_inbox;
pub mod models;
pub mod publish;
pub mod rate_limit;
pub mod scheduler;
pub mod state;
pub mod turnstile;
pub mod webhooks;

use migration::{Migrator, MigratorTrait};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection, DbErr, TransactionTrait};

/// Any fixed number: every brain process asks Postgres for this same advisory lock.
const MIGRATION_LOCK: i64 = 0x736f_6369_616c; // "social"

/// Connects to `DATABASE_URL` and applies pending migrations.
///
/// Two processes starting together would both try to create the migrations table, and one
/// would die with Postgres 42710. So one waits: the lock lives in a transaction on its own
/// connection, held while the migrations run on the pool, and Postgres frees it on commit or
/// if this process dies.
pub async fn connect(config: &config::Config) -> Result<DatabaseConnection, DbErr> {
    let db = Database::connect(&config.database_url).await?;
    let lock = db.begin().await?;
    lock.execute_unprepared(&format!("SELECT pg_advisory_xact_lock({MIGRATION_LOCK})"))
        .await?;
    Migrator::up(&db, None).await?;
    lock.commit().await?;
    Ok(db)
}
