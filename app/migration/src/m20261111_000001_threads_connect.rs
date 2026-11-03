use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// An OAuth connect attempt (Threads now) shares oauth_states with Google sign-in: `provider`
// names it, and it remembers the team and user that started it, so the callback attaches the
// account to that team. Google rows keep all three NULL. post_media is looked up by its storage
// key when a platform downloads a file from its brain link.
const UP: &str = r#"ALTER TABLE oauth_states
  ADD COLUMN provider text,
  ADD COLUMN team_id uuid REFERENCES teams (id) ON DELETE CASCADE,
  ADD COLUMN user_id uuid REFERENCES users (id) ON DELETE CASCADE;
CREATE INDEX post_media_storage_key_idx ON post_media (storage_key);"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP INDEX IF EXISTS post_media_storage_key_idx; \
                   DELETE FROM oauth_states WHERE provider IS NOT NULL; \
                   ALTER TABLE oauth_states DROP COLUMN IF EXISTS user_id, \
                   DROP COLUMN IF EXISTS team_id, DROP COLUMN IF EXISTS provider";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
