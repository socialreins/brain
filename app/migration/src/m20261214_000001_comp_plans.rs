use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Every team that exists before billing is switched on keeps everything: a comped Scheduler and
// Automation plan (`provider = 'comp'`, no Stripe, no end), so turning `BILLING_ENABLED` on locks
// nobody out (decision 0006, Harun 2026-11-23). A team with no row, or with only an ended one,
// gets it; a live plan (paid or trial) is left alone. Teams made later get one from an admin
// (`PUT /v1/admin/teams/{id}/plan`).
const UP: &str = r#"
INSERT INTO subscriptions (team_id, plan_id, status, provider)
SELECT id, 'automation', 'active', 'comp' FROM teams
ON CONFLICT (team_id) DO UPDATE SET plan_id = 'automation', status = 'active', provider = 'comp',
  provider_subscription_id = NULL, trial_ends_at = NULL, current_period_end = NULL,
  cancel_at_period_end = false, updated_at = now()
WHERE subscriptions.status = 'canceled';"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DELETE FROM subscriptions WHERE provider = 'comp'";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
