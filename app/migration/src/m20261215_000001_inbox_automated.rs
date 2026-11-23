use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// `automated_at`: when automations last ran over an inbox item to the end. A new item has none
// until then, so the inbox tick's sweep runs automations again on recent items a crash, a
// restart or a database error left behind (runs are unique per automation and item, so nothing
// sends twice). Items already stored count as done.
const UP: &str = r#"
ALTER TABLE inbox_items ADD COLUMN automated_at timestamptz;
UPDATE inbox_items SET automated_at = now();
CREATE INDEX inbox_items_unautomated_idx ON inbox_items (received_at) WHERE automated_at IS NULL;"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP INDEX IF EXISTS inbox_items_unautomated_idx;
                   ALTER TABLE inbox_items DROP COLUMN IF EXISTS automated_at";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
