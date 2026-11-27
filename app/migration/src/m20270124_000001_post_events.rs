use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// A real log of what happened to a post: one row per step the publisher took, so a post's page
// shows a claim, a retry or a failed account as its own line.
// The kinds are a short, fixed list (`post_events::KINDS`); `detail` is our own words or a code,
// never post text or a secret. Rows go with their post (cascade; a target's events go with it
// too), so there is no purge of its own. The index serves a post's newest first page.
const UP: &str = "CREATE TABLE post_events (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    team_id uuid NOT NULL REFERENCES teams(id) ON DELETE CASCADE,
    post_id uuid NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
    target_id uuid REFERENCES post_targets(id) ON DELETE CASCADE,
    kind text NOT NULL CHECK (kind IN (
        'target_claimed', 'attempt_started', 'published', 'failed', 'retry_scheduled',
        'late_to_draft', 'taken_down', 'comment_posted', 'comment_failed', 'cover_set',
        'cover_failed')),
    detail text,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX post_events_post_idx ON post_events (post_id, created_at DESC, id DESC);";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS post_events;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
