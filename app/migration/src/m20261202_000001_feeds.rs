use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// RSS and Atom feeds that create posts on their own (at most 20 per team, kept by the app under
// a per-team advisory lock). `failures` counts failed checks in a row; `etag` and
// `last_modified` are what the feed last answered, sent back as a conditional GET.
// `feed_items` holds one row per item of a feed, by the hash of its guid: `seen` once it was
// posted (or was there when the feed was added), else `failed` counts its refused creates.
const UP: &str = r#"CREATE TABLE feeds (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
  social_set_id uuid NOT NULL REFERENCES social_sets(id) ON DELETE CASCADE,
  url text NOT NULL,
  platforms jsonb NOT NULL,
  template text NOT NULL,
  mode text NOT NULL CHECK (mode IN ('draft', 'queue', 'now')),
  active boolean NOT NULL DEFAULT true,
  etag text,
  last_modified text,
  last_checked_at timestamptz,
  last_error text,
  failures integer NOT NULL DEFAULT 0,
  items_posted integer NOT NULL DEFAULT 0,
  created_by uuid REFERENCES users(id) ON DELETE SET NULL,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX feeds_team ON feeds (team_id, created_at, id);
CREATE TABLE feed_items (
  feed_id uuid NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
  guid_hash text NOT NULL,
  failed integer NOT NULL DEFAULT 0,
  seen boolean NOT NULL DEFAULT true,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (feed_id, guid_hash));"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS feed_items; DROP TABLE IF EXISTS feeds;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
