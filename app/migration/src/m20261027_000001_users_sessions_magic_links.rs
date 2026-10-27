use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Emails are stored lowercased, so the plain UNIQUE is case-insensitive; the CHECK keeps it so.
// current_team_id gets its foreign key with the teams table.
// Tokens are never stored, only their SHA-256 hex.
const UP: &str = r#"CREATE TABLE users (
  id uuid PRIMARY KEY,
  email text NOT NULL UNIQUE CHECK (email = lower(email)),
  name text NOT NULL,
  username text UNIQUE,
  current_team_id uuid,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE sessions (
  token_hash text PRIMARY KEY,
  user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  expires_at timestamptz NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX sessions_user_id_idx ON sessions (user_id);
CREATE INDEX sessions_expires_at_idx ON sessions (expires_at);
CREATE TABLE magic_links (
  token_hash text PRIMARY KEY,
  email text NOT NULL CHECK (email = lower(email)),
  expires_at timestamptz NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX magic_links_email_idx ON magic_links (email);
CREATE INDEX magic_links_expires_at_idx ON magic_links (expires_at);"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS magic_links, sessions, users";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
