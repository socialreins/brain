use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Files uploaded from the user's device for posts (decision 0008, Harun 2026-11-23). The file
// is in the media store under `storage_key` (`uploads/<team>/<id>`); a post names it by its
// brain link (`url`). The hourly sweep deletes one that is a day old and that no draft,
// scheduled or queued post of its team names.
const UP: &str = r#"CREATE TABLE media_uploads (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  storage_key text NOT NULL UNIQUE,
  url text NOT NULL,
  content_type text NOT NULL,
  size bigint NOT NULL,
  created_by uuid REFERENCES users (id) ON DELETE SET NULL,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX media_uploads_team_idx ON media_uploads (team_id, created_at);"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE media_uploads";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
