use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Post metrics: the latest counts of one published target, no history. `fetched_at` is the last
// try (a claim sets it before the network is asked), `attempts` the failed tries in a row and
// `error` why the last one failed; `gone` is a post the platform no longer has, never read again.
const UP: &str = "CREATE TABLE post_metrics (
  target_id uuid PRIMARY KEY REFERENCES post_targets (id) ON DELETE CASCADE,
  post_id uuid NOT NULL REFERENCES posts (id) ON DELETE CASCADE,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  platform text NOT NULL,
  likes bigint,
  comments bigint,
  shares bigint,
  views bigint,
  fetched_at timestamptz NOT NULL,
  error text,
  attempts integer NOT NULL DEFAULT 0);
CREATE INDEX post_metrics_post_idx ON post_metrics (post_id);";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS post_metrics";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
