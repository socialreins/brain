use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Thread posts: a post's follow-up parts (`[{body, media}]`), each a reply to the one before.
// Per target, the root stays in remote_id (plus remote_cid, which a reply names) and every
// published part is appended to parts (`[{remote_id, cid, remote_url, published_at}]`) the
// moment the platform takes it, so a cut-off run shows how far it got.
const UP: &str = "ALTER TABLE posts ADD COLUMN thread jsonb NOT NULL DEFAULT '[]'
  CHECK (jsonb_typeof(thread) = 'array');
ALTER TABLE post_targets ADD COLUMN remote_cid text,
  ADD COLUMN parts jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(parts) = 'array');";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE posts DROP COLUMN IF EXISTS thread;
ALTER TABLE post_targets DROP COLUMN IF EXISTS remote_cid, DROP COLUMN IF EXISTS parts;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
