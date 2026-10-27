use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// password_hash is an Argon2id PHC string; NULL means the user signs in by link or passkey only.
// passkey_challenges holds WebAuthn ceremony state for 5 minutes: user_id is set for a
// registration and NULL for a sign-in. Each row is used once.
const UP: &str = r#"ALTER TABLE users ADD COLUMN password_hash text;
CREATE TABLE passkeys (
  id uuid PRIMARY KEY,
  user_id uuid NOT NULL REFERENCES users (id) ON DELETE CASCADE,
  credential_id bytea NOT NULL UNIQUE,
  credential jsonb NOT NULL,
  name text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  last_used_at timestamptz);
CREATE INDEX passkeys_user_id_idx ON passkeys (user_id);
CREATE TABLE passkey_challenges (
  id uuid PRIMARY KEY,
  user_id uuid REFERENCES users (id) ON DELETE CASCADE,
  state jsonb NOT NULL,
  expires_at timestamptz NOT NULL);
CREATE INDEX passkey_challenges_expires_at_idx ON passkey_challenges (expires_at);"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS passkey_challenges, passkeys; \
                   ALTER TABLE users DROP COLUMN IF EXISTS password_hash";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
