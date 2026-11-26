use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// When the inbox tick last claimed a Bluesky account for a read: the tick takes the accounts read
// longest ago first and stamps them, so the round no longer needs a lock that holds a database
// connection while every account is read. Null means never (a new connection goes first).
const UP: &str = "ALTER TABLE social_accounts ADD COLUMN inbox_read_at timestamptz;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE social_accounts DROP COLUMN IF EXISTS inbox_read_at;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
