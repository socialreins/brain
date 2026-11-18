use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Newsletter sign-ups from public post pages: `teams.newsletter` switches the form on, and
// `subscribers` holds one row per team and email. `verify_token_hash` is the SHA-256 hex of the
// token in the latest confirmation email; `verified_at` is set when its link was confirmed.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE teams ADD COLUMN newsletter boolean NOT NULL DEFAULT false;
CREATE TABLE subscribers (
    id uuid PRIMARY KEY,
    team_id uuid NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    email text NOT NULL,
    page_code text,
    verify_token_hash text UNIQUE,
    verify_sent_at timestamptz,
    verified_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (team_id, email)
);";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS subscribers;
ALTER TABLE teams DROP COLUMN IF EXISTS newsletter;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
