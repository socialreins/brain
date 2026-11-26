use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// "Report this page" on `/p/<code>` (security review, finding 7): a report row per click-through, and
// `taken_down_at` for an admin to switch a page off. A team cannot switch it back: its own
// `enabled` is not touched by a takedown.
const UP: &str = r#"ALTER TABLE post_pages ADD COLUMN taken_down_at timestamptz;
CREATE TABLE page_reports (
  id uuid PRIMARY KEY,
  page_id uuid NOT NULL REFERENCES post_pages (id) ON DELETE CASCADE,
  reason text NOT NULL CHECK (reason IN ('phishing', 'spam', 'illegal', 'other')),
  message text NOT NULL DEFAULT '',
  contact text,
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX page_reports_page_idx ON page_reports (page_id, created_at DESC);
CREATE INDEX page_reports_created_idx ON page_reports (created_at DESC)"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE page_reports; ALTER TABLE post_pages DROP COLUMN taken_down_at";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
