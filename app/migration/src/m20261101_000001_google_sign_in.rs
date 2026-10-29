use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Password sign-in is gone: the hashes go with it. Google sign-in links an account by its
// stable `sub`. oauth_states holds one sign-in attempt for 10 minutes and is deleted on use;
// only the state's SHA-256 is stored, the browser holds the state in a cookie.
const UP: &str = r#"ALTER TABLE users DROP COLUMN IF EXISTS password_hash;
ALTER TABLE users ADD COLUMN google_sub text UNIQUE;
CREATE TABLE oauth_states (
  state_hash text PRIMARY KEY,
  nonce text NOT NULL,
  verifier text NOT NULL,
  next text NOT NULL,
  expires_at timestamptz NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX oauth_states_expires_at_idx ON oauth_states (expires_at);"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS oauth_states; \
                   ALTER TABLE users DROP COLUMN IF EXISTS google_sub; \
                   ALTER TABLE users ADD COLUMN IF NOT EXISTS password_hash text";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
