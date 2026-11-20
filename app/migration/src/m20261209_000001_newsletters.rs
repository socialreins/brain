use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Newsletters: one email to every confirmed subscriber of a team who did not unsubscribe.
// `teams.newsletter_sender_*` is who it comes from, with the postal address the law asks for
// (both set or both null). `subscribers.unsubscribed_at` keeps an address out of every send;
// the token of a subscriber's unsubscribe links is made the first time a mail goes out to them
// (so existing rows need no backfill), kept as its SHA-256 (`unsubscribe_token_hash`, what a
// link is looked up by) and sealed under `ENCRYPTION_KEY` (`unsubscribe_token`), so later mails
// carry the same link and older ones keep working. A start copies the sender onto the
// newsletter and snapshots the audience into `newsletter_recipients` (one row per newsletter
// and subscriber); a recipient moves `pending` to `sending` (claimed) to `mailing` (the mail
// call is about to start) to an end state and never back from `mailing`, so it is mailed at
// most once. A `paused` newsletter waits for a person (`pause_reason`), and
// `resumed_at` is where its run of failures starts counting again. Broadcast recipients get
// the same in-between state, `delivering`.
const UP: &str = r#"ALTER TABLE teams ADD COLUMN newsletter_sender_name text,
  ADD COLUMN newsletter_sender_address text,
  ADD CONSTRAINT teams_newsletter_sender_check
    CHECK ((newsletter_sender_name IS NULL) = (newsletter_sender_address IS NULL));
ALTER TABLE subscribers ADD COLUMN unsubscribed_at timestamptz,
  ADD COLUMN unsubscribe_token_hash text UNIQUE,
  ADD COLUMN unsubscribe_token bytea;
CREATE TABLE newsletters (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  subject text NOT NULL,
  body text NOT NULL,
  status text NOT NULL DEFAULT 'draft'
    CHECK (status IN ('draft', 'sending', 'paused', 'sent', 'cancelled')),
  pause_reason text,
  tested_at timestamptz,
  sender_name text,
  sender_address text,
  created_by uuid REFERENCES users (id) ON DELETE SET NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  started_at timestamptz,
  resumed_at timestamptz,
  finished_at timestamptz,
  CONSTRAINT newsletters_sender_check CHECK (status = 'draft'
    OR (sender_name IS NOT NULL AND sender_address IS NOT NULL)));
CREATE INDEX newsletters_team_idx ON newsletters (team_id, created_at DESC, id DESC);
CREATE TABLE newsletter_recipients (
  id uuid PRIMARY KEY,
  newsletter_id uuid NOT NULL REFERENCES newsletters (id) ON DELETE CASCADE,
  subscriber_id uuid REFERENCES subscribers (id) ON DELETE SET NULL,
  status text NOT NULL DEFAULT 'pending'
    CHECK (status IN ('pending', 'sending', 'mailing', 'sent', 'skipped', 'failed')),
  reason text,
  updated_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (newsletter_id, subscriber_id));
CREATE INDEX newsletter_recipients_status_idx
  ON newsletter_recipients (newsletter_id, status, id);
ALTER TABLE broadcast_recipients DROP CONSTRAINT broadcast_recipients_status_check,
  ADD CONSTRAINT broadcast_recipients_status_check
    CHECK (status IN ('pending', 'sending', 'delivering', 'sent', 'skipped', 'failed'));"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "UPDATE broadcast_recipients SET status = 'sending' WHERE status = 'delivering';
ALTER TABLE broadcast_recipients DROP CONSTRAINT broadcast_recipients_status_check,
  ADD CONSTRAINT broadcast_recipients_status_check
    CHECK (status IN ('pending', 'sending', 'sent', 'skipped', 'failed'));
DROP TABLE IF EXISTS newsletter_recipients;
DROP TABLE IF EXISTS newsletters;
ALTER TABLE subscribers DROP COLUMN IF EXISTS unsubscribe_token,
  DROP COLUMN IF EXISTS unsubscribe_token_hash, DROP COLUMN IF EXISTS unsubscribed_at;
ALTER TABLE teams DROP CONSTRAINT IF EXISTS teams_newsletter_sender_check,
  DROP COLUMN IF EXISTS newsletter_sender_address, DROP COLUMN IF EXISTS newsletter_sender_name;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
