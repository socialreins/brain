use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// One automatic retry (decision 0008, Harun 2026-11-23): a target whose publish failed with an
// error that certainly sent nothing (`connectors::UNSENT`) goes back to `pending` once, with
// `retry_at` about 5 minutes on. `retried` records that it was.
const UP: &str = "ALTER TABLE post_targets ADD COLUMN retry_at timestamptz, \
                  ADD COLUMN retried boolean NOT NULL DEFAULT false;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE post_targets DROP COLUMN retry_at, DROP COLUMN retried;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
