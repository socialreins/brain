use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Mail about posts that did not go out (decision 0008, Harun 2026-11-23): one row per post and
// kind (`failed`, `partial`, `late`), queued with the change that causes it and claimed by
// setting `sent_at` before it is mailed (`post_mail`).
const UP: &str = "CREATE TABLE post_notices (
    post_id uuid NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    team_id uuid NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN ('failed', 'partial', 'late')),
    created_at timestamptz NOT NULL DEFAULT now(),
    sent_at timestamptz,
    PRIMARY KEY (post_id, kind)
);
CREATE INDEX post_notices_unsent ON post_notices (created_at) WHERE sent_at IS NULL;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS post_notices;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
