use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// team_user.role: owner, admin or member. Invitations store only the token's SHA-256 hex and
// never grant owner. social_sets is the minimal shape teams need now: one Default set per team,
// pinned by the partial unique index; the rest of the sets feature comes later.
const UP: &str = r#"CREATE TABLE teams (
  id uuid PRIMARY KEY,
  name text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now());
CREATE TABLE team_user (
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  role text NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
  created_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (team_id, user_id));
CREATE INDEX team_user_user_id_idx ON team_user (user_id);
CREATE TABLE team_invitations (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  email text NOT NULL CHECK (email = lower(email)),
  role text NOT NULL CHECK (role IN ('admin', 'member')),
  token_hash text NOT NULL UNIQUE,
  invited_by uuid REFERENCES users (id) ON DELETE SET NULL,
  expires_at timestamptz NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (team_id, email));
CREATE INDEX team_invitations_email_idx ON team_invitations (email);
ALTER TABLE users ADD CONSTRAINT users_current_team_id_fkey
  FOREIGN KEY (current_team_id) REFERENCES teams (id) ON DELETE SET NULL;
CREATE TABLE social_sets (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  name text NOT NULL,
  is_default boolean NOT NULL DEFAULT false,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  deleted_at timestamptz);
CREATE INDEX social_sets_team_id_idx ON social_sets (team_id);
CREATE UNIQUE INDEX social_sets_one_default_idx ON social_sets (team_id) WHERE is_default;"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE users DROP CONSTRAINT IF EXISTS users_current_team_id_fkey; \
                   DROP TABLE IF EXISTS social_sets, team_invitations, team_user, teams";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
