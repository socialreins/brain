use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// A flow run parked at a `wait_for_reply` step: `awaiting_reply` until the person's next DM
// (`reply_item_id`, set when it comes, which also brings `resume_at` forward) or the timeout.
const UP: &str = "ALTER TABLE automation_runs
  ADD COLUMN awaiting_reply boolean NOT NULL DEFAULT false,
  ADD COLUMN reply_item_id uuid REFERENCES inbox_items(id) ON DELETE SET NULL;
CREATE INDEX automation_runs_awaiting_reply ON automation_runs (team_id)
  WHERE awaiting_reply AND status = 'waiting';";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP INDEX IF EXISTS automation_runs_awaiting_reply;
ALTER TABLE automation_runs DROP COLUMN IF EXISTS reply_item_id,
  DROP COLUMN IF EXISTS awaiting_reply;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
