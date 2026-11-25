use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Every slug a team gave up (changed, cleared, or its team deleted), one row per retirement.
// The row does two jobs: for 90 days after `retired_at` no other team may take the slug, and for
// 400 days the web app redirects `/t/<slug>/...` to the team's current address. No foreign key:
// a deleted team's rows outlive it, so its slug stays held. It also counts a team's slug
// changes in the last 30 days. Replaces `team_slug_holds`, whose rows move over.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "CREATE TABLE team_slug_history (
    id bigserial PRIMARY KEY,
    slug text NOT NULL,
    team_id uuid NOT NULL,
    retired_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX team_slug_history_slug ON team_slug_history (slug, retired_at DESC);
CREATE INDEX team_slug_history_team ON team_slug_history (team_id, retired_at DESC);
INSERT INTO team_slug_history (slug, team_id, retired_at)
    SELECT slug, team_id, released_at FROM team_slug_holds;
DROP TABLE team_slug_holds;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "CREATE TABLE team_slug_holds (
    slug text PRIMARY KEY,
    team_id uuid NOT NULL,
    released_at timestamptz NOT NULL DEFAULT now()
);
INSERT INTO team_slug_holds (slug, team_id, released_at)
    SELECT DISTINCT ON (slug) slug, team_id, retired_at FROM team_slug_history
    ORDER BY slug, retired_at DESC;
DROP TABLE team_slug_history;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
