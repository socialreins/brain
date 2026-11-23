use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// An inbox holds accounts (`inbox_accounts`); an account may be in several inboxes. "All" holds
// none and shows every item of the team. Inbox-capable accounts are Bluesky, Facebook and
// Instagram. Each custom inbox keeps what its old scope matched: its `account_ids`, the accounts of
// its live set, or every capable account of the team, narrowed by its old `platforms`. Then the
// scope and `platforms` leave `filters`. A team without a custom inbox gets "Inbox" with every
// capable account.
const UP: &str = r#"
CREATE TABLE inbox_accounts (
  inbox_id uuid NOT NULL REFERENCES inboxes (id) ON DELETE CASCADE,
  social_account_id uuid NOT NULL REFERENCES social_accounts (id) ON DELETE CASCADE,
  PRIMARY KEY (inbox_id, social_account_id));
CREATE INDEX inbox_accounts_account_idx ON inbox_accounts (social_account_id);
INSERT INTO inbox_accounts (inbox_id, social_account_id)
  SELECT i.id, a.id FROM inboxes i JOIN social_accounts a ON a.team_id = i.team_id
  WHERE NOT i.is_all AND a.provider IN ('bluesky', 'facebook', 'instagram')
    AND (jsonb_array_length(COALESCE(i.filters->'platforms', '[]')) = 0
         OR i.filters->'platforms' ? a.provider)
    AND CASE
      WHEN i.filters->>'social_set_id' IS NOT NULL THEN EXISTS (
        SELECT 1 FROM social_account_social_set m JOIN social_sets s ON s.id = m.social_set_id
        WHERE m.social_account_id = a.id AND s.deleted_at IS NULL
          AND s.id = (i.filters->>'social_set_id')::uuid)
      WHEN jsonb_array_length(COALESCE(i.filters->'account_ids', '[]')) > 0
        THEN i.filters->'account_ids' ? a.id::text
      ELSE true
    END;
UPDATE inboxes SET filters = filters - 'social_set_id' - 'account_ids' - 'platforms';
WITH made AS (
  INSERT INTO inboxes (id, team_id, name)
    SELECT gen_random_uuid(), t.id, 'Inbox' FROM teams t
    WHERE NOT EXISTS (SELECT 1 FROM inboxes i WHERE i.team_id = t.id AND NOT i.is_all)
    RETURNING id, team_id)
INSERT INTO inbox_accounts (inbox_id, social_account_id)
  SELECT made.id, a.id FROM made JOIN social_accounts a ON a.team_id = made.team_id
  WHERE a.provider IN ('bluesky', 'facebook', 'instagram');"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        // The old scope is not rebuilt: inboxes go back to every account of the team.
        let sql = "DROP TABLE IF EXISTS inbox_accounts";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
