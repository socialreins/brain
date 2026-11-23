use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// A repeat whose next copy could not be made is mailed about too (`post_mail::Kind::RepeatEnded`).
const UP: &str = "ALTER TABLE post_notices DROP CONSTRAINT post_notices_kind_check;
ALTER TABLE post_notices ADD CONSTRAINT post_notices_kind_check
  CHECK (kind IN ('failed', 'partial', 'late', 'repeat_ended'));";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DELETE FROM post_notices WHERE kind = 'repeat_ended';
ALTER TABLE post_notices DROP CONSTRAINT post_notices_kind_check;
ALTER TABLE post_notices ADD CONSTRAINT post_notices_kind_check
  CHECK (kind IN ('failed', 'partial', 'late'));";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
