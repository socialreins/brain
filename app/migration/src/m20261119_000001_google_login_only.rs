use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE oauth_states ADD COLUMN login_only boolean NOT NULL DEFAULT false;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE oauth_states DROP COLUMN IF EXISTS login_only;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
