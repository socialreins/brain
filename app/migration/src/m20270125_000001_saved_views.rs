use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Saved filters ("views") of a list page, one set per person and team: `scope` says which page
// (`comments` now, `inbox` later), `filters` is the page's own query (`{"network": "x", ...}`,
// strings only; the web page decides what keys mean). At most one view per person, team and scope
// is the default (the partial unique index), and a name is unique per person, team and scope
// ignoring case. Rows go with their team or their person.
const UP: &str = "CREATE TABLE saved_views (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    team_id uuid NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    scope text NOT NULL CHECK (scope IN ('comments', 'inbox')),
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 60),
    filters jsonb NOT NULL DEFAULT '{}'::jsonb,
    is_default boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX saved_views_name ON saved_views (team_id, user_id, scope, lower(name));
CREATE UNIQUE INDEX saved_views_default ON saved_views (team_id, user_id, scope) WHERE is_default;
CREATE INDEX saved_views_user ON saved_views (user_id);";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS saved_views;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
