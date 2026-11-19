use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Repeating posts: `posts.repeat` is null or `{"every", "interval", "until", "remaining"}` (and
// `day`, the day of the month a monthly rule aims for after a short month). The publish claim
// reads it once, makes the next copy and clears it. The index serves the per-team cap.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE posts ADD COLUMN repeat jsonb;
CREATE INDEX posts_repeat ON posts (team_id) WHERE repeat IS NOT NULL;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP INDEX IF EXISTS posts_repeat;
ALTER TABLE posts DROP COLUMN IF EXISTS repeat;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
