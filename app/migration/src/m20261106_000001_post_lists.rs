use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Ordered groups of posts. A list is team-wide (social_set_id null) or tied to one set, whose
// posts alone it then holds. Positions are 1-based and rewritten whole by every item write
// (delete then insert, under the list row lock), so they stay dense; reads renumber over the
// posts the caller can see. remote_ids is kept for platform playlists. Deleting a list sets
// deleted_at and keeps its posts.
const UP: &str = r#"CREATE TABLE post_lists (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  social_set_id uuid REFERENCES social_sets (id) ON DELETE CASCADE,
  name text NOT NULL,
  remote_ids jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(remote_ids) = 'object'),
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  deleted_at timestamptz);
CREATE INDEX post_lists_team_idx ON post_lists (team_id) WHERE deleted_at IS NULL;
CREATE TABLE post_list_items (
  list_id uuid NOT NULL REFERENCES post_lists (id) ON DELETE CASCADE,
  post_id uuid NOT NULL REFERENCES posts (id) ON DELETE CASCADE,
  position integer NOT NULL CHECK (position > 0),
  PRIMARY KEY (list_id, post_id),
  UNIQUE (list_id, position));
CREATE INDEX post_list_items_post_idx ON post_list_items (post_id);"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS post_list_items, post_lists";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
