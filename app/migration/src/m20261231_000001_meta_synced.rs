use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// When `meta_inbox::backfill` last started reading a Facebook or Instagram account's recent
// comments and conversations; null means never, which is how a new connection is picked up.
const UP: &str = "ALTER TABLE social_accounts ADD COLUMN meta_synced_at timestamptz;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE social_accounts DROP COLUMN IF EXISTS meta_synced_at;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
