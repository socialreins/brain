use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// An optional name for a waitlist signup, set once after joining with a token the join
// answered (only its SHA-256 hex is stored). Existing rows keep `verified_at` as it is: it is
// only ever set by opening the emailed link, so nobody becomes verified by this migration.
const UP: &str = "ALTER TABLE waitlist_signups ADD COLUMN name text,
  ADD COLUMN name_token_hash text UNIQUE;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE waitlist_signups DROP COLUMN IF EXISTS name,
  DROP COLUMN IF EXISTS name_token_hash;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
