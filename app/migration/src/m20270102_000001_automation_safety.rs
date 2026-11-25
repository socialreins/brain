use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// `cooldown_minutes`: one person triggers an automation at most once in this long.
// `daily_cap`: the automation starts at most this many runs a day. `automations_paused_at`: the
// team switched every automation off at once (null: not paused); items that come meanwhile are
// not answered later.
const UP: &str = "ALTER TABLE automations
  ADD COLUMN cooldown_minutes integer CHECK (cooldown_minutes BETWEEN 1 AND 43200),
  ADD COLUMN daily_cap integer CHECK (daily_cap BETWEEN 1 AND 500);
ALTER TABLE teams ADD COLUMN automations_paused_at timestamptz;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE automations DROP COLUMN IF EXISTS cooldown_minutes,
  DROP COLUMN IF EXISTS daily_cap;
ALTER TABLE teams DROP COLUMN IF EXISTS automations_paused_at;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
