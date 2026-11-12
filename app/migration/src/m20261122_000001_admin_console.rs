use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// `waitlist_signups.approved_at`: an admin let this email sign up whatever the sign-up rules
// say; existing rows stay unapproved. `signup_settings`: at most one row, the sign-up rules
// saved from the admin console; while it is empty REGISTRATION_DISABLED and SIGNUP_DOMAINS apply.
const UP: &str = "ALTER TABLE waitlist_signups ADD COLUMN approved_at timestamptz;
CREATE TABLE signup_settings (
  id boolean PRIMARY KEY DEFAULT true CHECK (id),
  mode text NOT NULL CHECK (mode IN ('open', 'closed', 'domains')),
  domains jsonb NOT NULL DEFAULT '[]',
  updated_at timestamptz NOT NULL DEFAULT now());";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS signup_settings;
ALTER TABLE waitlist_signups DROP COLUMN IF EXISTS approved_at;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
