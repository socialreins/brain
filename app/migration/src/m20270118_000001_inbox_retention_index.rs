use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// The daily inbox retention purge (`scheduler::purge_old_inbox_items`) deletes items by
// `created_at`; without an index every batch of 1000 scans the table, so the purge costs more
// the more history a team keeps. One transaction like every other migration, so not
// `CONCURRENTLY` (see `m20270111_000001_scaling_indexes`); a few seconds at a million rows, and
// `lock_timeout` fails this boot instead of queueing every writer behind it.
const UP: &str = "SET LOCAL lock_timeout = '10s';
CREATE INDEX inbox_items_created_idx ON inbox_items (created_at);";

const DOWN: &str = "DROP INDEX IF EXISTS inbox_items_created_idx;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection()
            .execute_unprepared(DOWN)
            .await
            .map(|_| ())
    }
}
