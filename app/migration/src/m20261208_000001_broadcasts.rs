use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Broadcasts: one DM to every reachable contact with a tag. `contact_opt_outs` holds who must
// never get one, by team, platform and the platform's id of the person, not by contact: it
// outlives a deleted contact, so a later item of the same person makes a contact that is still
// opted out. `contacts.last_broadcast_at` is when a broadcast last went out to the contact (one
// a day at most). A start snapshots the audience into `broadcast_recipients` (one row per
// broadcast and contact), and a recipient only ever moves `pending` to `sending` to an end
// state, so it is sent at most once. One broadcast per team is `sending` at a time; a `paused`
// one waits for a person (`pause_reason`), and `resumed_at` is where its run of failures
// starts counting again.
const UP: &str = r#"CREATE TABLE contact_opt_outs (
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  platform text NOT NULL,
  external_id text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (team_id, platform, external_id));
ALTER TABLE contacts ADD COLUMN last_broadcast_at timestamptz;
CREATE TABLE broadcasts (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  name text NOT NULL,
  text text NOT NULL,
  tag text NOT NULL,
  platform text,
  status text NOT NULL DEFAULT 'draft'
    CHECK (status IN ('draft', 'sending', 'paused', 'sent', 'cancelled')),
  pause_reason text,
  created_by uuid REFERENCES users (id) ON DELETE SET NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  started_at timestamptz,
  resumed_at timestamptz,
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

/// The backfill, safe to run again: of each person's DMs that say exactly `stop`, `unsubscribe`
/// or `start` (trimmed, any case), the latest decides, as `inbox::store` decides from now on.
// It reads every DM once: split it into batches if `inbox_items` grows.
pub const BACKFILL_OPT_OUTS: &str =
    "INSERT INTO contact_opt_outs (team_id, platform, external_id) \
    SELECT team_id, platform, author_id FROM (SELECT DISTINCT ON (team_id, platform, author_id) \
    team_id, platform, author_id, lower(btrim(text, E' \\t\\n\\r')) AS said FROM inbox_items \
    WHERE kind = 'dm' AND lower(btrim(text, E' \\t\\n\\r')) IN ('stop', 'unsubscribe', 'start') \
    ORDER BY team_id, platform, author_id, received_at DESC, id DESC) l \
    WHERE l.said <> 'start' ON CONFLICT DO NOTHING";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        for sql in [UP, BACKFILL_OPT_OUTS] {
            m.get_connection().execute_unprepared(sql).await?;
        }
        Ok(())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS broadcast_recipients;
DROP TABLE IF EXISTS broadcasts;
ALTER TABLE contacts DROP COLUMN IF EXISTS last_broadcast_at;
DROP TABLE IF EXISTS contact_opt_outs;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
