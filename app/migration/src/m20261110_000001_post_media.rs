use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// A post's media, downloaded at publish time into brain's storage (local disk or a bucket) and
// deleted once the post settles. No foreign key: a deleted post leaves its rows behind for the
// sweep, which deletes the stored file before the row.
const UP: &str = "CREATE TABLE post_media (
  id uuid PRIMARY KEY,
  post_id uuid NOT NULL,
  source_url text NOT NULL,
  storage_key text NOT NULL,
  content_type text NOT NULL,
  size bigint NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (post_id, source_url)
);";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS post_media;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
