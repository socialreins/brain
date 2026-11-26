use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// One row per account a person deleted themselves (decision 0019). Counts only: no user id,
// email or team id, so the row is no personal data and outlives everything it describes.
const UP: &str = r#"CREATE TABLE account_deletions (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  deleted_at timestamptz NOT NULL DEFAULT now(),
  teams_deleted integer NOT NULL,
  teams_left integer NOT NULL,
  subscriptions_cancelled integer NOT NULL
)"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection()
            .execute_unprepared("DROP TABLE account_deletions")
            .await
            .map(|_| ())
    }
}
