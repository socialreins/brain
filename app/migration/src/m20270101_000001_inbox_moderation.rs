use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// What a person did to a Facebook or Instagram comment from the inbox: `{hidden, liked, deleted}`,
// only the keys that were set. Meta's webhook does not say, so this is what SocialReins did.
const UP: &str = "ALTER TABLE inbox_items ADD COLUMN moderation jsonb NOT NULL DEFAULT '{}';";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE inbox_items DROP COLUMN IF EXISTS moderation;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
