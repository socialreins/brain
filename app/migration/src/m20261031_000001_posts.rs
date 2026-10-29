use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// One post per platform; one target per account it goes to. A target's remote_id is saved the
// moment the platform accepts the post, and a target is only claimed while it has none, so a
// post is never sent twice. Targets outlive a disconnected account (social_account_id goes null)
// so published history stays. quota_reserved is set in the same transaction that records the
// post's hit against the team's daily publish cap; nothing publishes without it.
const UP: &str = r#"CREATE TABLE posts (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  social_set_id uuid NOT NULL REFERENCES social_sets (id) ON DELETE CASCADE,
  platform text NOT NULL CHECK (platform IN
    ('facebook', 'instagram', 'threads', 'linkedin', 'bluesky', 'x', 'tiktok', 'youtube')),
  body text NOT NULL DEFAULT '',
  media jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(media) = 'array'),
  status text NOT NULL CHECK (status IN
    ('draft', 'scheduled', 'queued', 'published', 'failed', 'deleted')),
  scheduled_at timestamptz,
  published_at timestamptz,
  error text,
  quota_reserved boolean NOT NULL DEFAULT false,
  created_by uuid REFERENCES users (id) ON DELETE SET NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  CHECK (status <> 'scheduled' OR scheduled_at IS NOT NULL));
CREATE INDEX posts_team_scheduled_idx ON posts (team_id, scheduled_at);
CREATE INDEX posts_set_idx ON posts (social_set_id);
CREATE INDEX posts_due_idx ON posts (scheduled_at) WHERE status = 'scheduled';
CREATE TABLE post_targets (
  id uuid PRIMARY KEY,
  post_id uuid NOT NULL REFERENCES posts (id) ON DELETE CASCADE,
  social_account_id uuid REFERENCES social_accounts (id) ON DELETE SET NULL,
  status text NOT NULL CHECK (status IN
    ('pending', 'publishing', 'published', 'failed', 'deleted')),
  remote_id text,
  remote_url text,
  error text,
  published_at timestamptz,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (post_id, social_account_id));
CREATE INDEX post_targets_account_idx ON post_targets (social_account_id);
CREATE INDEX post_targets_publishing_idx ON post_targets (updated_at)
  WHERE status = 'publishing';"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS post_targets, posts";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
