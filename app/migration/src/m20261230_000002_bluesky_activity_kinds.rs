use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Likes and reposts of an account's posts are stored as quiet inbox items (`like`, `repost`),
// `done` from the start like follows, and listed only as activity.
const UP: &str = "ALTER TABLE inbox_items DROP CONSTRAINT inbox_items_kind_check,
  ADD CONSTRAINT inbox_items_kind_check
    CHECK (kind IN ('comment', 'mention', 'dm', 'follow', 'like', 'repost'));";

const DOWN: &str = "DELETE FROM inbox_items WHERE kind IN ('like', 'repost');
ALTER TABLE inbox_items DROP CONSTRAINT inbox_items_kind_check,
  ADD CONSTRAINT inbox_items_kind_check CHECK (kind IN ('comment', 'mention', 'dm', 'follow'));";

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
