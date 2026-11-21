use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// The tables of Cloud billing. Nothing reads them unless `BILLING_ENABLED=true`, so a
// self-hosted install is unchanged.
// `plans`: what a plan allows. A NULL limit means unlimited. `automation` covers the inbox,
// automations, broadcasts and newsletters. Prices live with the payment provider, not here.
// `subscriptions`: one row per team, written by the provider's webhook.
// `credit_ledger`: prepaid credits for what costs per use (`x` posts, `ai` calls). A positive
// amount is a grant, a negative one a spend; a team's balance is the sum. `ref` is an
// idempotency key, unique per team, kind and direction, so a retried grant or spend never counts
// twice and a refund may reuse the ref of the spend it undoes.
const UP: &str = r#"CREATE TABLE plans (
  id text PRIMARY KEY,
  name text NOT NULL,
  max_accounts integer,
  max_members integer,
  automation boolean NOT NULL,
  max_contacts integer,
  created_at timestamptz NOT NULL DEFAULT now());
INSERT INTO plans (id, name, max_accounts, max_members, automation, max_contacts) VALUES
  ('scheduler', 'Scheduler', 10, 3, false, NULL),
  ('automation', 'Scheduler and Automation', 25, NULL, true, 2500);
CREATE TABLE subscriptions (
  team_id uuid PRIMARY KEY REFERENCES teams (id) ON DELETE CASCADE,
  plan_id text NOT NULL REFERENCES plans (id),
  status text NOT NULL CHECK (status IN ('trialing', 'active', 'past_due', 'canceled')),
  provider text,
  provider_customer_id text,
  provider_subscription_id text,
  trial_ends_at timestamptz,
  current_period_end timestamptz,
  cancel_at_period_end boolean NOT NULL DEFAULT false,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now());
CREATE UNIQUE INDEX subscriptions_provider_subscription_idx
  ON subscriptions (provider, provider_subscription_id) WHERE provider_subscription_id IS NOT NULL;
CREATE TABLE credit_ledger (
  id uuid PRIMARY KEY,
  team_id uuid NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
  kind text NOT NULL CHECK (kind IN ('x', 'ai')),
  amount bigint NOT NULL CHECK (amount <> 0),
  reason text NOT NULL,
  ref text,
  created_at timestamptz NOT NULL DEFAULT now());
CREATE INDEX credit_ledger_team_kind_idx ON credit_ledger (team_id, kind);
CREATE UNIQUE INDEX credit_ledger_ref_idx ON credit_ledger (team_id, kind, ref, (amount > 0)) WHERE ref IS NOT NULL;"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP TABLE IF EXISTS credit_ledger, subscriptions, plans";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
