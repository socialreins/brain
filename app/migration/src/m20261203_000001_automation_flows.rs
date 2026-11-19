use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Automation flows. An automation holds `actions` or, in `steps`, an ordered list of sends and
// waits. A run of a flow keeps the step it last tried (`step_index`), the md5 of the steps it
// started on (`steps_hash`, so it never goes on with other steps) and, while it waits
// (`status = 'waiting'`), when it goes on (`resume_at`). A Meta quick reply click is a `dm`
// item with the button's payload, which runs `button_clicked` automations.
const UP: &str = "ALTER TABLE automations ADD COLUMN steps jsonb,
  DROP CONSTRAINT automations_trigger_check,
  ADD CONSTRAINT automations_trigger_check CHECK (trigger IN ('button_clicked',
    'comment_received', 'dm_received', 'mention_received', 'new_follower', 'story_reply'));
ALTER TABLE automation_runs ADD COLUMN step_index integer,
  ADD COLUMN resume_at timestamptz,
  ADD COLUMN steps_hash text,
  DROP CONSTRAINT automation_runs_status_check,
  ADD CONSTRAINT automation_runs_status_check
    CHECK (status IN ('running', 'waiting', 'sent', 'failed', 'skipped'));
CREATE INDEX automation_runs_waiting_idx ON automation_runs (resume_at)
  WHERE status = 'waiting';
ALTER TABLE inbox_items ADD COLUMN button_payload text;";

const DOWN: &str = "ALTER TABLE inbox_items DROP COLUMN IF EXISTS button_payload;
DROP INDEX IF EXISTS automation_runs_waiting_idx;
DELETE FROM automation_runs WHERE status = 'waiting';
ALTER TABLE automation_runs DROP CONSTRAINT automation_runs_status_check,
  ADD CONSTRAINT automation_runs_status_check
    CHECK (status IN ('running', 'sent', 'failed', 'skipped')),
  DROP COLUMN IF EXISTS step_index,
  DROP COLUMN IF EXISTS resume_at,
  DROP COLUMN IF EXISTS steps_hash;
DELETE FROM automations WHERE trigger = 'button_clicked' OR steps IS NOT NULL;
ALTER TABLE automations DROP CONSTRAINT automations_trigger_check,
  ADD CONSTRAINT automations_trigger_check CHECK (trigger IN ('comment_received',
    'dm_received', 'mention_received', 'new_follower', 'story_reply')),
  DROP COLUMN IF EXISTS steps;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection()
            .execute_unprepared(DOWN)
            .await
            .map(|_| ())
    }
}
