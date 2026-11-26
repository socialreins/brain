use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// When the publish loop last ran, and the last stretch it missed (`down_from` to `down_to`):
// brain's own downtime. `post_lateness` is how late a post is once that stretch is taken off,
// so an outage of ours never turns the posts due in it into drafts (decision 0008).
const UP: &str = "CREATE TABLE scheduler_ticks (
  name text PRIMARY KEY,
  at timestamptz NOT NULL,
  down_from timestamptz,
  down_to timestamptz);
CREATE FUNCTION post_lateness(due timestamptz) RETURNS interval LANGUAGE sql STABLE AS $$
  SELECT now() - due - GREATEST(
    LEAST(now(), COALESCE(w.down_to, now())) - GREATEST(due, COALESCE(w.down_from, now())),
    interval '0')
  FROM (SELECT 1) one LEFT JOIN scheduler_ticks w ON w.name = 'publish'
$$;";

const DOWN: &str = "DROP FUNCTION IF EXISTS post_lateness(timestamptz);
DROP TABLE IF EXISTS scheduler_ticks;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection()
            .execute_unprepared(DOWN)
            .await
            .map(|_| ())
    }
}
