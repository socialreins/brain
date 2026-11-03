use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// The Facebook Pages (with their tokens) a Meta login granted, sealed with ENCRYPTION_KEY, while
// the user picks which become accounts: one pending pick per user and team, for 15 minutes.
const UP: &str = r#"CREATE TABLE meta_picks (
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  pages bytea NOT NULL,
  expires_at timestamptz NOT NULL,
  PRIMARY KEY (team_id, user_id)
);"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS meta_picks";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
