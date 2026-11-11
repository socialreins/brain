use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// `api_tokens.read_only`: the token may only read (GET and HEAD on `/v1`, read-only `/mcp`
// tools). Existing tokens keep writing.
const UP: &str = "ALTER TABLE api_tokens ADD COLUMN read_only boolean NOT NULL DEFAULT false;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE api_tokens DROP COLUMN IF EXISTS read_only;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
