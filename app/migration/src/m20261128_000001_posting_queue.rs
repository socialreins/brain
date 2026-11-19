use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// The posting queue: one weekly schedule per social set. `slots` is a sorted JSON array of
// minutes of the week (`weekday * 1440 + minute_of_day`, weekday 0 is Sunday), at most 50, read
// in the IANA zone `time_zone`. A post added to the queue takes the earliest free slot.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "CREATE TABLE queue_schedules (
    social_set_id uuid PRIMARY KEY REFERENCES social_sets(id) ON DELETE CASCADE,
    team_id uuid NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    time_zone text NOT NULL,
    slots jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(slots) = 'array' AND jsonb_array_length(slots) <= 50),
    updated_at timestamptz NOT NULL DEFAULT now()
);";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS queue_schedules;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
