use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// How often a run was tried again after the platform refused a send for rate limiting (nothing
// was sent, so trying again is safe), by itself or by a person.
const UP: &str = "ALTER TABLE automation_runs ADD COLUMN retries integer NOT NULL DEFAULT 0;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE automation_runs DROP COLUMN IF EXISTS retries;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
