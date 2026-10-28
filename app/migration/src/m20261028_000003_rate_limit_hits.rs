use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// One row per counted action, for daily caps that must survive restarts and deletes of what
// they count (e.g. invitations per recipient). Rows older than a day are swept on use.
const UP: &str = r#"CREATE TABLE rate_limit_hits (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  bucket text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX rate_limit_hits_bucket_idx ON rate_limit_hits (bucket, created_at);
CREATE INDEX rate_limit_hits_created_at_idx ON rate_limit_hits (created_at);"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS rate_limit_hits";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
