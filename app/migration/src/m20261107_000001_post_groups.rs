use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// A post group: the posts one `POST /v1/post-groups` call made, one per platform, sharing a
// group_id. Single posts keep it null.
const UP: &str = "ALTER TABLE posts ADD COLUMN group_id uuid;
CREATE INDEX posts_team_group_idx ON posts (team_id, group_id) WHERE group_id IS NOT NULL;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE posts DROP COLUMN IF EXISTS group_id";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
