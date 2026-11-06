use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// `posts.fanned_out`: the post's targets were created, one per account of its platform in its set
// at that time. A retry re-queues existing targets only, so an account added to the set later
// never gets a target (and a post) it was not part of.
const UP: &str = "ALTER TABLE posts ADD COLUMN fanned_out boolean NOT NULL DEFAULT false; \
    UPDATE posts SET fanned_out = true WHERE EXISTS \
    (SELECT 1 FROM post_targets t WHERE t.post_id = posts.id);";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE posts DROP COLUMN IF EXISTS fanned_out;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
