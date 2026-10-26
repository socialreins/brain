use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Emails are stored lowercased by the app; the CHECK keeps that true for any other writer.
const UP: &str = r#"CREATE TABLE waitlist_signups (
  id uuid PRIMARY KEY,
  email text NOT NULL UNIQUE CHECK (email = lower(email)),
  source text,
  created_at timestamptz NOT NULL DEFAULT now());"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS waitlist_signups";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
