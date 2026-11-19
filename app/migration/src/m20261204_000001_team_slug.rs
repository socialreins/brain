use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Team slugs: an optional address a team picks for itself, beside its permanent `code`. Stored
// lowercase; the unique index is on `lower(slug)` so no two teams hold one in any case.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE teams ADD COLUMN slug text;
CREATE UNIQUE INDEX teams_slug_key ON teams (lower(slug));";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE teams DROP COLUMN IF EXISTS slug;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
