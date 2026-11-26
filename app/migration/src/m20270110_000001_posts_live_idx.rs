use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// The post cap counts the posts of a team that still need something done, a group once
// (`controllers::posts::live_posts`). History (published, deleted) is most of a team's rows after
// a while, so the count reads this partial index, never the history. `lock_timeout`: the index
// build waits for open writers on `posts`, and everything behind it waits for the build; a timeout
// fails this boot instead.
const UP: &str = "SET LOCAL lock_timeout = '10s';
CREATE INDEX posts_live_idx ON posts (team_id, (COALESCE(group_id, id)))
  WHERE status IN ('draft', 'scheduled', 'queued', 'failed');";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP INDEX IF EXISTS posts_live_idx;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
