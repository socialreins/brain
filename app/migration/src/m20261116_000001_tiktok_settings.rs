use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// TikTok per-post settings (`posts.tiktok`, null for none): the privacy level the user chose, the
// comment, duet and stitch switches, the commercial content disclosure and the AI-generated flag.
const UP: &str = "ALTER TABLE posts ADD COLUMN tiktok jsonb;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE posts DROP COLUMN IF EXISTS tiktok;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
