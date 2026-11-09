use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE magic_links ADD COLUMN name text;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE magic_links DROP COLUMN IF EXISTS name;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
