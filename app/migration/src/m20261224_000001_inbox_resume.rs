use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Where a round of an account's inbox read stopped when its time ran out or it hit a cap, so
// the next tick carries on: `notifications_cursor` is the page to read next and
// `notifications_pending_at` the newest item that round saw (it becomes `notifications_seen_at`
// once the round reaches the end); `messages_resume_at` is the last time of the newest
// conversation read so far (conversations are read oldest first).
const UP: &str = "ALTER TABLE social_accounts
  ADD COLUMN notifications_cursor text,
  ADD COLUMN notifications_pending_at timestamptz,
  ADD COLUMN messages_resume_at timestamptz;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE social_accounts
  DROP COLUMN IF EXISTS notifications_cursor,
  DROP COLUMN IF EXISTS notifications_pending_at,
  DROP COLUMN IF EXISTS messages_resume_at;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
