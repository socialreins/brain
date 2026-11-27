use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// What a refund or a dispute took back of a credit pack (`charge.refunded`, `charge.dispute.created`).
// One row per Stripe event, so a delivery seen twice takes back once. `owed` is what the event
// asked for, `taken` what the team still had (never below a zero balance); the rest was already
// spent. Rows per `payment_intent` add up, so several partial refunds and then a dispute never
// take back more than the pack. The operator digest counts the last day's rows.
//
// The grant of a pack now names its payment (`reason` = `x pack <payment intent>`), so a refund
// finds its team and credits from the ledger; `credit_ledger_pack_idx` serves that lookup.
const UP: &str = r#"CREATE TABLE credit_takebacks (
  event_id text PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  payment_intent text NOT NULL,
  kind text NOT NULL CHECK (kind IN ('refund', 'dispute')),
  owed bigint NOT NULL CHECK (owed > 0),
  taken bigint NOT NULL CHECK (taken >= 0 AND taken <= owed),
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX credit_takebacks_payment_idx ON credit_takebacks (payment_intent);
CREATE INDEX credit_takebacks_created_idx ON credit_takebacks (created_at);
CREATE INDEX credit_ledger_pack_idx ON credit_ledger (reason) WHERE kind = 'x' AND amount > 0"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql =
            "DROP INDEX IF EXISTS credit_ledger_pack_idx; DROP TABLE IF EXISTS credit_takebacks";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
