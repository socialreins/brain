use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Video cover: a post's optional cover image URL (`posts.cover`, null for none). Instagram takes
// it with the video; Facebook and YouTube get it after the video is live, so a refusal there
// cannot fail the post and is kept per target as `cover_error` (null: set, or none to set).
const UP: &str = "ALTER TABLE posts ADD COLUMN cover text;
ALTER TABLE post_targets ADD COLUMN cover_error text;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE posts DROP COLUMN IF EXISTS cover;
ALTER TABLE post_targets DROP COLUMN IF EXISTS cover_error;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
