use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// One outbound webhook per team; its signing secret is sealed with ENCRYPTION_KEY. Deliveries
// are an outbox: a target's status change inserts its row in the same statement, and UNIQUE
// (target_id, event) makes each target fire each event once. Deleting the webhook drops its
// deliveries. target_id has no foreign key, so a later hard delete of the post keeps its events.
const UP: &str = r#"CREATE TABLE team_webhooks (
  team_id uuid PRIMARY KEY REFERENCES teams (id) ON DELETE CASCADE,
  url text NOT NULL CHECK (char_length(url) <= 2000),
  secret bytea NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE webhook_deliveries (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES team_webhooks (team_id) ON DELETE CASCADE,
  target_id uuid NOT NULL,
  event text NOT NULL,
  payload jsonb NOT NULL,
  attempts integer NOT NULL DEFAULT 0,
  next_attempt_at timestamptz NOT NULL DEFAULT now(),
  delivered_at timestamptz,
  last_status integer,
  last_error text,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (target_id, event));
CREATE INDEX webhook_deliveries_due_idx ON webhook_deliveries (next_attempt_at)
  WHERE delivered_at IS NULL;
CREATE INDEX webhook_deliveries_team_idx ON webhook_deliveries (team_id);"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS webhook_deliveries, team_webhooks";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
