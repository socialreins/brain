use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Inboxes and automations (decision 0004). Items come from any platform: Bluesky polling now,
// a Meta webhook later, both into inbox_items, unique per account and remote id so ingest is
// idempotent. An item keeps what a reply needs: its own remote id and cid, its thread root, and
// for a DM the conversation. Inbox filters and automation conditions and actions are typed JSON
// (checked by the controllers), so new kinds need no migration. A run is claimed (inserted)
// before anything is sent and never retried; quota_reserved mirrors posts. The *_seen_at
// columns are the ingest cursors: the newest notification and message already read.
const UP: &str = r#"ALTER TABLE social_accounts
  ADD COLUMN dm_access boolean NOT NULL DEFAULT false,
  ADD COLUMN notifications_seen_at timestamptz,
  ADD COLUMN messages_seen_at timestamptz,
  ADD COLUMN inbox_error_at timestamptz;
CREATE TABLE inbox_items (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  social_account_id uuid NOT NULL REFERENCES social_accounts (id) ON DELETE CASCADE,
  platform text NOT NULL,
  kind text NOT NULL CHECK (kind IN ('comment', 'mention', 'dm')),
  remote_id text NOT NULL,
  remote_cid text,
  parent_remote_id text,
  root_remote_id text,
  root_remote_cid text,
  convo_id text,
  post_id uuid REFERENCES posts (id) ON DELETE SET NULL,
  author_id text NOT NULL,
  author_handle text,
  author_name text,
  author_avatar text,
  text text NOT NULL DEFAULT '',
  received_at timestamptz NOT NULL,
  state text NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'done')),
  replied_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (social_account_id, remote_id));
CREATE INDEX inbox_items_team_received_idx ON inbox_items (team_id, received_at DESC, id DESC);
CREATE TABLE inbox_replies (
  id uuid PRIMARY KEY,
  inbox_item_id uuid NOT NULL REFERENCES inbox_items (id) ON DELETE CASCADE,
  kind text NOT NULL CHECK (kind IN ('public', 'dm')),
  text text NOT NULL,
  remote_id text,
  sent_by uuid REFERENCES users (id) ON DELETE SET NULL,
  automation_id uuid,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX inbox_replies_item_idx ON inbox_replies (inbox_item_id);
CREATE TABLE inboxes (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  name text NOT NULL,
  is_all boolean NOT NULL DEFAULT false,
  filters jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(filters) = 'object'),
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX inboxes_team_idx ON inboxes (team_id);
CREATE UNIQUE INDEX inboxes_one_all_idx ON inboxes (team_id) WHERE is_all;
INSERT INTO inboxes (id, team_id, name, is_all)
  SELECT gen_random_uuid(), id, 'All', true FROM teams;
CREATE TABLE automations (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  name text NOT NULL,
  active boolean NOT NULL DEFAULT true,
  activated_at timestamptz NOT NULL DEFAULT now(),
  trigger text NOT NULL CHECK (trigger IN ('comment_received')),
  conditions jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(conditions) = 'object'),
  actions jsonb NOT NULL CHECK (jsonb_typeof(actions) = 'array'),
  fallback_public_reply text,
  created_by uuid REFERENCES users (id) ON DELETE SET NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX automations_team_idx ON automations (team_id);
ALTER TABLE inbox_replies ADD CONSTRAINT inbox_replies_automation_fkey
  FOREIGN KEY (automation_id) REFERENCES automations (id) ON DELETE SET NULL;
CREATE TABLE automation_runs (
  id uuid PRIMARY KEY,
  automation_id uuid NOT NULL REFERENCES automations (id) ON DELETE CASCADE,
  inbox_item_id uuid NOT NULL REFERENCES inbox_items (id) ON DELETE CASCADE,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  status text NOT NULL CHECK (status IN ('running', 'sent', 'failed', 'skipped')),
  reason text,
  results jsonb NOT NULL DEFAULT '[]',
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (automation_id, inbox_item_id));
CREATE INDEX automation_runs_item_idx ON automation_runs (inbox_item_id);
CREATE INDEX automation_runs_running_idx ON automation_runs (updated_at)
  WHERE status = 'running';"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS automation_runs, inbox_replies, automations, inboxes, \
                   inbox_items; ALTER TABLE social_accounts DROP COLUMN IF EXISTS dm_access, \
                   DROP COLUMN IF EXISTS notifications_seen_at, \
                   DROP COLUMN IF EXISTS messages_seen_at, DROP COLUMN IF EXISTS inbox_error_at";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
