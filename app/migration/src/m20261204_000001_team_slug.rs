use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Team slugs: an optional address a team picks for itself, beside its permanent `code`. Stored
// lowercase; the unique index is on `lower(slug)` so no two teams hold one in any case.
// `team_slug_holds` keeps a slug a team gave up for that team alone, 30 days from `released_at`,
// so old links cannot land on another team. No foreign key: a hold outlives a deleted team.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE teams ADD COLUMN slug text;
CREATE UNIQUE INDEX teams_slug_key ON teams (lower(slug));
CREATE TABLE team_slug_holds (
    slug text PRIMARY KEY,
    team_id uuid NOT NULL,
    released_at timestamptz NOT NULL DEFAULT now()
);";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS team_slug_holds;
ALTER TABLE teams DROP COLUMN IF EXISTS slug;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
