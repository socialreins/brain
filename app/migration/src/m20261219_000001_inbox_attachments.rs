use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// `attachments`: the photos, videos, stickers and links an inbox item carries, as
// [{kind, url, preview_url?}] with https links only (`inbox::Attachment`). Older items have none.
const UP: &str = "ALTER TABLE inbox_items ADD COLUMN attachments jsonb NOT NULL DEFAULT '[]'";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE inbox_items DROP COLUMN IF EXISTS attachments";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
