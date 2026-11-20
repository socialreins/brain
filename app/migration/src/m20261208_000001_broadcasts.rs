use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Broadcasts: one DM to every reachable contact with a tag. `contacts.opted_out` keeps a person
// out of every broadcast. A start snapshots the audience into `broadcast_recipients` (one row
// per broadcast and contact), and a recipient only ever moves `pending` to `sending` to an end
// state, so it is sent at most once. One broadcast per team is `sending` at a time.
const UP: &str = r#"ALTER TABLE contacts ADD COLUMN opted_out boolean NOT NULL DEFAULT false;
CREATE TABLE broadcasts (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  name text NOT NULL,
  text text NOT NULL,
  tag text NOT NULL,
  platform text,
  status text NOT NULL DEFAULT 'draft'
    CHECK (status IN ('draft', 'sending', 'sent', 'cancelled')),
  created_by uuid REFERENCES users (id) ON DELETE SET NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  started_at timestamptz,
  finished_at timestamptz);
CREATE INDEX broadcasts_team_idx ON broadcasts (team_id, created_at DESC, id DESC);
CREATE UNIQUE INDEX broadcasts_one_sending_idx ON broadcasts (team_id) WHERE status = 'sending';
CREATE TABLE broadcast_recipients (
  id uuid PRIMARY KEY,
  broadcast_id uuid NOT NULL REFERENCES broadcasts (id) ON DELETE CASCADE,
  contact_id uuid REFERENCES contacts (id) ON DELETE SET NULL,
  status text NOT NULL DEFAULT 'pending'
    CHECK (status IN ('pending', 'sending', 'sent', 'skipped', 'failed')),
  reason text,
  updated_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (broadcast_id, contact_id));
CREATE INDEX broadcast_recipients_status_idx ON broadcast_recipients (broadcast_id, status, id);"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS broadcast_recipients;
DROP TABLE IF EXISTS broadcasts;
ALTER TABLE contacts DROP COLUMN IF EXISTS opted_out;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
