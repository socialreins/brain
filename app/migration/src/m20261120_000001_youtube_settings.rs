use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// YouTube per-post settings (`posts.youtube`, null for none): the visibility the user chose.
// Null or no `privacy` uploads as private.
const UP: &str = "ALTER TABLE posts ADD COLUMN youtube jsonb;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE posts DROP COLUMN IF EXISTS youtube;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
