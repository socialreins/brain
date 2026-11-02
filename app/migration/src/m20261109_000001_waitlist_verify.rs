use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Waitlist email confirmation: the emailed link carries a token whose SHA-256 hex is stored in
// verify_token_hash; verify_sent_at throttles resends. Existing rows stay unverified.
const UP: &str = "ALTER TABLE waitlist_signups ADD COLUMN verified_at timestamptz,
  ADD COLUMN verify_token_hash text UNIQUE,
  ADD COLUMN verify_sent_at timestamptz;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE waitlist_signups DROP COLUMN IF EXISTS verified_at,
  DROP COLUMN IF EXISTS verify_token_hash, DROP COLUMN IF EXISTS verify_sent_at;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
