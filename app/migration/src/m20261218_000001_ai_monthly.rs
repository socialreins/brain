use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// A monthly allowance of AI replies on our key (decision 0006, Harun 2026-11-23): `plans.ai_monthly`
// replies a calendar month (UTC), Scheduler 0, Scheduler and Automation 200. Each use is a
// `credit_ledger` row of kind `ai_plan` (-1, under the reply's ref, +1 when given back), so it
// takes the same team lock and idempotent ref as `ai` credits; `ai_plan` is never a balance.
const UP: &str = r#"ALTER TABLE plans ADD COLUMN ai_monthly integer NOT NULL DEFAULT 0;
UPDATE plans SET ai_monthly = 200 WHERE id = 'automation';
ALTER TABLE credit_ledger DROP CONSTRAINT credit_ledger_kind_check;
ALTER TABLE credit_ledger ADD CONSTRAINT credit_ledger_kind_check
  CHECK (kind IN ('x', 'ai', 'ai_plan'));"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DELETE FROM credit_ledger WHERE kind = 'ai_plan';
            ALTER TABLE credit_ledger DROP CONSTRAINT credit_ledger_kind_check;
            ALTER TABLE credit_ledger ADD CONSTRAINT credit_ledger_kind_check
              CHECK (kind IN ('x', 'ai'));
            ALTER TABLE plans DROP COLUMN ai_monthly";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
