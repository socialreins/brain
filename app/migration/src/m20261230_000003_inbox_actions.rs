use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// What people do from an inbox item on the platform (like, repost, follow, mute, block, hide a
// reply, delete our reply, report), one row per attempt, kept after the item or account goes.
const UP: &str = "CREATE TABLE inbox_actions (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  inbox_item_id uuid REFERENCES inbox_items (id) ON DELETE SET NULL,
  social_account_id uuid REFERENCES social_accounts (id) ON DELETE SET NULL,
  action text NOT NULL,
  target text NOT NULL,
  status text NOT NULL CHECK (status IN ('done', 'failed')),
  remote_id text,
  detail jsonb NOT NULL DEFAULT '{}',
  error text,
  done_by uuid REFERENCES users (id) ON DELETE SET NULL,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX inbox_actions_item_idx ON inbox_actions (inbox_item_id, created_at DESC);";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS inbox_actions;")
            .await
            .map(|_| ())
    }
}
