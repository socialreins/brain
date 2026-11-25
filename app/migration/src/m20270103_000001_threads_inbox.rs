use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// When `threads_inbox` last started reading a Threads account's replies and mentions (null: never,
// which is how a new connection is picked up and read for the first time), and when it last read
// the whole 7 days instead of only the recent posts.
const UP: &str = "ALTER TABLE social_accounts ADD COLUMN threads_synced_at timestamptz, \
ADD COLUMN threads_full_at timestamptz;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE social_accounts DROP COLUMN IF EXISTS threads_synced_at, \
                   DROP COLUMN IF EXISTS threads_full_at;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
