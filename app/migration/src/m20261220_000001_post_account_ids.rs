use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// The accounts a post goes to (decision 0008, Harun 2026-11-23): a JSON list of social account
// ids of its platform in its set, or null for every one of them, as before.
const UP: &str = "ALTER TABLE posts ADD COLUMN account_ids jsonb";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE posts DROP COLUMN account_ids";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
