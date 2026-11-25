use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Why an automation did not run on an item that reached it: the codes of the conditions that
// failed, or `wrong_trigger`, `not_yet_on`, `followed_before`, `no_public_page`,
// `stopped_by_other`. Runs skipped for a safety reason (paused, cool-down, caps) are already
// rows of `automation_runs`, so only the rest is kept here. Pruned after 14 days, and to the
// newest 100 rows of each kind per automation. The two triggers for a like and a repost of one
// of the account's posts (inbox items of kind `like` and `repost`) are allowed on automations.
const UP: &str = "ALTER TABLE automations DROP CONSTRAINT automations_trigger_check,
  ADD CONSTRAINT automations_trigger_check CHECK (trigger IN ('button_clicked',
    'comment_received', 'dm_received', 'like_received', 'mention_received', 'new_follower',
    'repost_received', 'story_mention', 'story_reply'));
CREATE TABLE automation_misses (
  automation_id uuid NOT NULL REFERENCES automations (id) ON DELETE CASCADE,
  inbox_item_id uuid NOT NULL REFERENCES inbox_items (id) ON DELETE CASCADE,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  reasons text[] NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (automation_id, inbox_item_id));
CREATE INDEX automation_misses_item_idx ON automation_misses (inbox_item_id);
CREATE INDEX automation_misses_auto_idx ON automation_misses (automation_id, created_at DESC);
CREATE INDEX automation_misses_created_idx ON automation_misses (created_at);";

const DOWN: &str = "DROP TABLE IF EXISTS automation_misses;
DELETE FROM automations WHERE trigger IN ('like_received', 'repost_received');
ALTER TABLE automations DROP CONSTRAINT automations_trigger_check,
  ADD CONSTRAINT automations_trigger_check CHECK (trigger IN ('button_clicked',
    'comment_received', 'dm_received', 'mention_received', 'new_follower', 'story_mention',
    'story_reply'));";

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
