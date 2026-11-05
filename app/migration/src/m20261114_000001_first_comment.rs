use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// First comment: a post's optional text (`posts.first_comment`, null for none), posted by the
// account right after the post is saved as published. The attempt is per target, because a
// post goes to several accounts: `comment_status` is null (not tried), `posting` (claimed, so
// it is never sent twice; one cut off midway stays here until recovery fails it), `posted` or
// `failed`, with the platform's comment id or why it failed. Failing never touches the target's
// own status.
const UP: &str = "ALTER TABLE posts ADD COLUMN first_comment text;
ALTER TABLE post_targets
  ADD COLUMN comment_status text CHECK (comment_status IN ('posting', 'posted', 'failed')),
  ADD COLUMN comment_id text,
  ADD COLUMN comment_error text;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE posts DROP COLUMN IF EXISTS first_comment;
ALTER TABLE post_targets DROP COLUMN IF EXISTS comment_status,
  DROP COLUMN IF EXISTS comment_id, DROP COLUMN IF EXISTS comment_error;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
