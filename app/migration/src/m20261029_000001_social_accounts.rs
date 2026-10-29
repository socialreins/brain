use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// One row per connected account, unique per team. access_token is sealed by
// `socialreins_core::crypto` (never plaintext); meta holds provider extras such as a Bluesky PDS.
// Every account also sits in its team's Default set; the controllers keep that true.
const UP: &str = r#"CREATE TABLE social_accounts (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  provider text NOT NULL,
  external_id text NOT NULL,
  handle text,
  display_name text,
  avatar_url text,
  access_token bytea NOT NULL,
  token_expires_at timestamptz,
  meta jsonb NOT NULL DEFAULT '{}',
  created_by uuid REFERENCES users (id) ON DELETE SET NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (team_id, provider, external_id));
CREATE TABLE social_account_social_set (
  social_account_id uuid NOT NULL REFERENCES social_accounts (id) ON DELETE CASCADE,
  social_set_id uuid NOT NULL REFERENCES social_sets (id) ON DELETE CASCADE,
  PRIMARY KEY (social_account_id, social_set_id));
CREATE INDEX social_account_social_set_set_idx ON social_account_social_set (social_set_id);"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS social_account_social_set, social_accounts";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
