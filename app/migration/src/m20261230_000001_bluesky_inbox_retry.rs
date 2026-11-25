use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// When a rate-limited inbox read may be tried again: the tick leaves the account alone until
// then, so a 429 is not hit every minute.
const UP: &str = "ALTER TABLE social_accounts ADD COLUMN inbox_retry_at timestamptz;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE social_accounts DROP COLUMN IF EXISTS inbox_retry_at;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
