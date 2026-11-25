use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// YouTube comments in the inbox. One row per channel that was ever switched on (off until a
// person opts in: the Google project's 10,000 units a day are shared by every team and by
// uploads). `daily_units` is what the channel may spend on reading a day (Pacific time, the day
// YouTube's quota resets on), `units_today` what it spent in `period`, `write_units_today` what
// replies and moderation cost it (shown, not limited). `newest_at` is the cursor: the newest
// top-level comment a finished sweep saw. A sweep cut short (time, pages) keeps its place in
// `page_token` and `pending_at` and goes on at the next tick. `paused_reason` stops polling until
// a person resumes it (`quota`, `errors`) or the day turns (`budget`).
// `youtube_threads` remembers how many replies a thread had when last read, so a thread with new
// ones is read again; `youtube_videos` is the title and thumbnail shown under a comment. Both are
// YouTube API data, kept at most 30 days (YouTube's developer policy) and then deleted.
const UP: &str = "CREATE TABLE youtube_inbox (
  social_account_id uuid PRIMARY KEY REFERENCES social_accounts (id) ON DELETE CASCADE,
  enabled boolean NOT NULL DEFAULT false,
  poll_minutes integer NOT NULL DEFAULT 30,
  daily_units integer NOT NULL DEFAULT 300,
  period date NOT NULL DEFAULT (now() AT TIME ZONE 'America/Los_Angeles')::date,
  units_today integer NOT NULL DEFAULT 0,
  write_units_today integer NOT NULL DEFAULT 0,
  newest_at timestamptz,
  page_token text,
  pending_at timestamptz,
  next_poll_at timestamptz NOT NULL DEFAULT now(),
  paused_reason text,
  last_read_at timestamptz,
  last_error text,
  errors integer NOT NULL DEFAULT 0,
  updated_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX youtube_inbox_due_idx ON youtube_inbox (next_poll_at) WHERE enabled;
CREATE TABLE youtube_threads (
  social_account_id uuid NOT NULL REFERENCES social_accounts (id) ON DELETE CASCADE,
  thread_id text NOT NULL,
  replies integer NOT NULL,
  seen_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (social_account_id, thread_id));
CREATE INDEX youtube_threads_seen_idx ON youtube_threads (seen_at);
CREATE TABLE youtube_videos (
  video_id text PRIMARY KEY,
  title text NOT NULL,
  thumbnail text,
  fetched_at timestamptz NOT NULL DEFAULT now());";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS youtube_videos; DROP TABLE IF EXISTS youtube_threads; \
                   DROP TABLE IF EXISTS youtube_inbox;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
