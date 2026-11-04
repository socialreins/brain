use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Meta items link to where they live (Bluesky's link is built from the item's at:// uri).
const UP: &str = "ALTER TABLE inbox_items ADD COLUMN permalink text";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE inbox_items DROP COLUMN IF EXISTS permalink";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
