use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// One row per rate limit bucket (`<prefix>:<key>`), for the short burst limits that used to be
// counted in process memory. `window_start` is when the current window began, `hits` how many
// checks it has counted, so one upsert sees the whole window across every replica. Rows are
// swept a day after their window began (longer than any window uses).
const UP: &str = r#"CREATE TABLE rate_limit_windows (
  bucket text PRIMARY KEY,
  window_start timestamptz NOT NULL,
  hits bigint NOT NULL);
CREATE INDEX rate_limit_windows_start_idx ON rate_limit_windows (window_start);"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS rate_limit_windows";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
