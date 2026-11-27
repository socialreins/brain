use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// An admin's own upload storage budget for a team, in bytes (decision 0024, Harun 2026-11-27).
// NULL uses `TEAM_STORAGE_BYTES` (`config.rs`); 0 is unlimited for that team. It caps the bytes
// of the team's `media_uploads` rows, not the size of one file (`MEDIA_MAX_BYTES`).
const UP: &str = "ALTER TABLE teams ADD COLUMN storage_budget_bytes bigint
  CONSTRAINT teams_storage_budget_nonnegative CHECK (storage_budget_bytes >= 0);";

const DOWN: &str = "ALTER TABLE teams DROP COLUMN IF EXISTS storage_budget_bytes;";

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
