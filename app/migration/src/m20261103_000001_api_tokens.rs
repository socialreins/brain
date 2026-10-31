use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// API tokens belong to a team, not a user; created_by only records who made one. A set token
// dies with its set (CASCADE, never SET NULL: a null set means a whole-team token). Only the
// SHA-256 hex of the secret is stored. Revoked and expired rows stay; the partial index serves
// the live-token count and list.
const UP: &str = r#"CREATE TABLE api_tokens (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  social_set_id uuid REFERENCES social_sets (id) ON DELETE CASCADE,
  name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 60),
  token_hash text NOT NULL UNIQUE,
  prefix_hint text NOT NULL,
  created_by uuid REFERENCES users (id) ON DELETE SET NULL,
  last_used_at timestamptz,
  expires_at timestamptz,
  revoked_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX api_tokens_team_live_idx ON api_tokens (team_id) WHERE revoked_at IS NULL;
CREATE INDEX api_tokens_set_live_idx ON api_tokens (social_set_id) WHERE revoked_at IS NULL;"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS api_tokens";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
