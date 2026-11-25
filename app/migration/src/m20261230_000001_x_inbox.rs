use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// X inbox settings and read state, one row per X account that was ever switched on. Reading X
// costs money (pay-per-use credits), so everything is off until a person opts in, and a row also
// keeps what the account may spend: `budget_micros` a calendar month (UTC), `spent_micros` so
// far in `period`. Amounts are millionths of a US dollar. `since_id` values are X's cursors, so
// a poll only fetches (and pays for) what is new. `paused_reason` stops polling until a person
// resumes it (`credits`, `usage_capped`, `errors`) or the month turns (`budget`).
// `units_owed` is items read and not yet paid for in team credits (billing on only).
const UP: &str = "CREATE TABLE x_inbox (
  social_account_id uuid PRIMARY KEY REFERENCES social_accounts (id) ON DELETE CASCADE,
  enabled boolean NOT NULL DEFAULT false,
  read_dms boolean NOT NULL DEFAULT false,
  poll_minutes integer NOT NULL DEFAULT 60,
  budget_micros bigint NOT NULL DEFAULT 2000000,
  spent_micros bigint NOT NULL DEFAULT 0,
  period date NOT NULL DEFAULT (date_trunc('month', now() AT TIME ZONE 'UTC'))::date,
  units_owed integer NOT NULL DEFAULT 0,
  mentions_since_id text,
  dm_since_id text,
  next_poll_at timestamptz NOT NULL DEFAULT now(),
  paused_reason text,
  last_read_at timestamptz,
  last_error text,
  errors integer NOT NULL DEFAULT 0,
  updated_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX x_inbox_due_idx ON x_inbox (next_poll_at) WHERE enabled;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS x_inbox;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
