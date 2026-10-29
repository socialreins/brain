pub mod config;
pub mod connectors;
pub mod controllers;
pub mod mail;
pub mod models;
pub mod publish;
pub mod rate_limit;
pub mod scheduler;
pub mod state;

use migration::{Migrator, MigratorTrait};
use sea_orm::{Database, DatabaseConnection, DbErr};

/// Connects to `DATABASE_URL` and applies pending migrations.
pub async fn connect(config: &config::Config) -> Result<DatabaseConnection, DbErr> {
    let db = Database::connect(&config.database_url).await?;
    Migrator::up(&db, None).await?;
    Ok(db)
}
