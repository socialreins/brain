use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// A failure notice whose mail could not be sent (every recipient failed) goes back to unsent
// and is tried again later, up to `post_mail::RETRIES` times: `tries` counts them, `retry_at`
// is when the next one is due.
const UP: &str = "ALTER TABLE post_notices
  ADD COLUMN tries int NOT NULL DEFAULT 0,
  ADD COLUMN retry_at timestamptz;";

const DOWN: &str =
    "ALTER TABLE post_notices DROP COLUMN IF EXISTS retry_at, DROP COLUMN IF EXISTS tries;";

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
