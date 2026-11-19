use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Team assets for the composer: media URLs kept for reuse (one row per team and URL, at most
// 1000 per team) and saved captions and hashtag sets (one name per team and kind, any case, at
// most 500 per team). The caps are kept by the app under a per-team advisory lock.
const UP: &str = r#"CREATE TABLE media_library (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
  url text NOT NULL,
  name text NOT NULL,
  created_by uuid REFERENCES users(id) ON DELETE SET NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (team_id, url));
CREATE INDEX media_library_team_newest ON media_library (team_id, created_at DESC, id DESC);
CREATE TABLE snippets (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
  kind text NOT NULL CHECK (kind IN ('caption', 'hashtags')),
  name text NOT NULL,
  body text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now());
CREATE UNIQUE INDEX snippets_team_kind_name ON snippets (team_id, kind, lower(name));"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS snippets; DROP TABLE IF EXISTS media_library;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
