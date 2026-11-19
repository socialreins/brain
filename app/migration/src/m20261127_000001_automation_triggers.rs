use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// More automation triggers. Bluesky follows are stored as inbox items of kind `follow`, `done`
// from the start so they never show as open work. An Instagram reply to a story is a `dm` with
// `story_reply` set and the story's id and link when Meta gives them. The partial index serves
// the open counts of `GET /v1/inboxes`.
const UP: &str = "ALTER TABLE inbox_items DROP CONSTRAINT inbox_items_kind_check,
  ADD CONSTRAINT inbox_items_kind_check CHECK (kind IN ('comment', 'mention', 'dm', 'follow')),
  ADD COLUMN story_reply boolean NOT NULL DEFAULT false,
  ADD COLUMN story_id text,
  ADD COLUMN story_url text;
CREATE INDEX inbox_items_open_idx ON inbox_items (team_id) WHERE state = 'open';
ALTER TABLE automations DROP CONSTRAINT automations_trigger_check,
  ADD CONSTRAINT automations_trigger_check CHECK (trigger IN ('comment_received',
    'dm_received', 'mention_received', 'new_follower', 'story_reply'));";

const DOWN: &str = "DELETE FROM automations WHERE trigger <> 'comment_received';
ALTER TABLE automations DROP CONSTRAINT automations_trigger_check,
  ADD CONSTRAINT automations_trigger_check CHECK (trigger IN ('comment_received'));
DROP INDEX IF EXISTS inbox_items_open_idx;
DELETE FROM inbox_items WHERE kind = 'follow';
ALTER TABLE inbox_items DROP CONSTRAINT inbox_items_kind_check,
  ADD CONSTRAINT inbox_items_kind_check CHECK (kind IN ('comment', 'mention', 'dm')),
  DROP COLUMN IF EXISTS story_reply,
  DROP COLUMN IF EXISTS story_id,
  DROP COLUMN IF EXISTS story_url;";

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
