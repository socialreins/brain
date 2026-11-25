use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Safety for automations. `teams.automations_paused` is the team's "pause all automations"
// switch: while on, no automation sends and a waiting run ends. `cooldown_minutes` keeps one
// automation from answering the same person again for that long, and `daily_cap` bounds one
// automation's runs a day (both null: off, only the team's daily cap applies).
const UP: &str = "ALTER TABLE teams ADD COLUMN automations_paused boolean NOT NULL DEFAULT false;
ALTER TABLE automations ADD COLUMN cooldown_minutes integer, ADD COLUMN daily_cap integer;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE automations DROP COLUMN IF EXISTS cooldown_minutes, \
            DROP COLUMN IF EXISTS daily_cap;
ALTER TABLE teams DROP COLUMN IF EXISTS automations_paused;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
