use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Alt text for post images: `posts.media_alt` is a JSON list of strings parallel to `media`
// (null: none); `post_targets.alt_error` says why a network did not get one, which never
// fails the post.
const UP: &str = "ALTER TABLE posts ADD COLUMN media_alt jsonb;
ALTER TABLE post_targets ADD COLUMN alt_error text;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE post_targets DROP COLUMN IF EXISTS alt_error;
ALTER TABLE posts DROP COLUMN IF EXISTS media_alt;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
