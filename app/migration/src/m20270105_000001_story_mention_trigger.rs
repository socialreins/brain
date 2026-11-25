use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// The `story_mention` trigger: an Instagram DM that is someone mentioning the account in a story.
const UP: &str = "ALTER TABLE automations DROP CONSTRAINT automations_trigger_check,
  ADD CONSTRAINT automations_trigger_check CHECK (trigger IN ('button_clicked',
    'comment_received', 'dm_received', 'mention_received', 'new_follower', 'story_mention',
    'story_reply'));";

const DOWN: &str = "DELETE FROM automations WHERE trigger = 'story_mention';
ALTER TABLE automations DROP CONSTRAINT automations_trigger_check,
  ADD CONSTRAINT automations_trigger_check CHECK (trigger IN ('button_clicked',
    'comment_received', 'dm_received', 'mention_received', 'new_follower', 'story_reply'));";

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
