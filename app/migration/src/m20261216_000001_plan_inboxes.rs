use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// `plans.max_inboxes`: custom inboxes a team may create ("All" never counts); NULL is unlimited.
// A downgrade keeps the inboxes a team has and only refuses new ones.
const UP: &str = r#"
ALTER TABLE plans ADD COLUMN max_inboxes integer;
UPDATE plans SET max_inboxes = 1 WHERE id = 'scheduler';
UPDATE plans SET max_inboxes = 10 WHERE id = 'automation';"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE plans DROP COLUMN IF EXISTS max_inboxes";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
