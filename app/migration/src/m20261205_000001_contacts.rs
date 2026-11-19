use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Contacts: one row per team, platform and author id, kept up by every stored inbox item
// (`inbox::store` upserts it in the item's own statement). Tags are a set per contact, at most 20,
// kept by the app under the contact's row lock. `inbox_items.assigned_to` is the member an item
// is handed to. Saved replies are snippets of a new kind, `reply`.
const UP: &str = r#"CREATE TABLE contacts (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  platform text NOT NULL,
  external_id text NOT NULL,
  handle text,
  display_name text,
  avatar_url text,
  notes text,
  first_seen_at timestamptz NOT NULL,
  last_seen_at timestamptz NOT NULL,
  UNIQUE (team_id, platform, external_id));
CREATE INDEX contacts_team_seen_idx ON contacts (team_id, last_seen_at DESC, id DESC);
CREATE TABLE contact_tags (
  contact_id uuid NOT NULL REFERENCES contacts (id) ON DELETE CASCADE,
  tag text NOT NULL,
  PRIMARY KEY (contact_id, tag));
ALTER TABLE inbox_items
  ADD COLUMN contact_id uuid REFERENCES contacts (id) ON DELETE SET NULL,
  ADD COLUMN assigned_to uuid REFERENCES users (id) ON DELETE SET NULL;
ALTER TABLE snippets DROP CONSTRAINT snippets_kind_check;
ALTER TABLE snippets ADD CONSTRAINT snippets_kind_check
  CHECK (kind IN ('caption', 'hashtags', 'reply'));"#;

/// The backfill, one statement per step, each safe to run again: a contact per author of the
/// items that have none yet (the newest handle, name and avatar that were given), then every
/// such item pointed at its contact. Items by the account itself were never stored, so every
/// author is a contact.
pub const BACKFILL_CONTACTS: &str = "INSERT INTO contacts (id, team_id, platform, external_id, \
    handle, display_name, avatar_url, first_seen_at, last_seen_at) \
    SELECT gen_random_uuid(), team_id, platform, author_id, \
    (array_agg(author_handle ORDER BY received_at DESC) FILTER (WHERE author_handle IS NOT NULL))[1], \
    (array_agg(author_name ORDER BY received_at DESC) FILTER (WHERE author_name IS NOT NULL))[1], \
    (array_agg(author_avatar ORDER BY received_at DESC) FILTER (WHERE author_avatar IS NOT NULL))[1], \
    min(received_at), max(received_at) FROM inbox_items WHERE contact_id IS NULL \
    GROUP BY team_id, platform, author_id \
    ON CONFLICT (team_id, platform, external_id) DO NOTHING";
pub const BACKFILL_ITEMS: &str = "UPDATE inbox_items i SET contact_id = c.id FROM contacts c \
    WHERE i.contact_id IS NULL AND c.team_id = i.team_id AND c.platform = i.platform \
    AND c.external_id = i.author_id";
// The backfill holds `inbox_items` for as long as it runs: split it into batches if the table grows.
// The index comes after it, so it is built once.
const INDEX: &str = "CREATE INDEX inbox_items_contact_idx ON inbox_items (contact_id)";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        for sql in [UP, BACKFILL_CONTACTS, BACKFILL_ITEMS, INDEX] {
            m.get_connection().execute_unprepared(sql).await?;
        }
        Ok(())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DELETE FROM snippets WHERE kind = 'reply';
ALTER TABLE snippets DROP CONSTRAINT snippets_kind_check;
ALTER TABLE snippets ADD CONSTRAINT snippets_kind_check CHECK (kind IN ('caption', 'hashtags'));
ALTER TABLE inbox_items DROP COLUMN IF EXISTS assigned_to, DROP COLUMN IF EXISTS contact_id;
DROP TABLE IF EXISTS contact_tags;
DROP TABLE IF EXISTS contacts;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
