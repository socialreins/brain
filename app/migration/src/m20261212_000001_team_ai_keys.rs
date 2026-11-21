use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// A team's own AI provider key (BYOK), for AI steps in automations and AI replies. `key` is
// sealed with the app cipher like account tokens; `last4` is the only part ever shown again.
const UP: &str = r#"
CREATE TABLE team_ai_keys (
  team_id uuid PRIMARY KEY REFERENCES teams (id) ON DELETE CASCADE,
  provider text NOT NULL CHECK (provider IN ('anthropic', 'openai')),
  key bytea NOT NULL,
  last4 text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now());"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS team_ai_keys";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
