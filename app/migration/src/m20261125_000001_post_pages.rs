use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Public pages: `teams.public_pages` switches them on for a team, and `post_pages` holds one
// page per post or post group. `subject` is the post's `group_id` when it has one, else its
// `id` (no foreign key, it names either); `code` is the page's public address, 10 characters
// of the team code alphabet, never changed.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE teams ADD COLUMN public_pages boolean NOT NULL DEFAULT false;
CREATE TABLE post_pages (
    id uuid PRIMARY KEY,
    team_id uuid NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    subject uuid NOT NULL UNIQUE,
    code text NOT NULL UNIQUE CHECK (code ~ '^[2-9a-hjkmnp-z]{10}$'),
    details text NOT NULL DEFAULT '',
    enabled boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS post_pages;
ALTER TABLE teams DROP COLUMN IF EXISTS public_pages;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
