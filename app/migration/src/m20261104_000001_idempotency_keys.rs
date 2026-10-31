use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// One row per Idempotency-Key a team's API tokens sent: `in_progress` while the first request
// runs, then `done` with the response to replay. Keys are per team, so tokens of one team share
// them; social_set_id is the set of a set token (NULL for a whole-team token), so a replay
// never crosses sets. The fingerprint is the SHA-256 hex of the method, path and raw body.
// Rows older than 30 days are purged hourly; the created_at index serves that purge.
const UP: &str = r#"CREATE TABLE idempotency_keys (
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  key text NOT NULL CHECK (char_length(key) BETWEEN 1 AND 255),
  method text NOT NULL,
  path text NOT NULL,
  fingerprint text NOT NULL,
  token_id uuid REFERENCES api_tokens (id) ON DELETE SET NULL,
  social_set_id uuid,
  status text NOT NULL DEFAULT 'in_progress' CHECK (status IN ('in_progress', 'done')),
  response_status smallint,
  response_body bytea,
  content_type text,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (team_id, key));
CREATE INDEX idempotency_keys_created_idx ON idempotency_keys (created_at);"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS idempotency_keys";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
