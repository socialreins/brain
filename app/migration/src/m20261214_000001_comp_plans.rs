use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Every team that exists before billing is switched on keeps everything: a comped Scheduler and
// Automation plan (`provider = 'comp'`, no Stripe, no end), so turning `BILLING_ENABLED` on locks
// nobody out (decision 0006, Harun 2026-11-23). Teams made later get one from an admin
// (`PUT /v1/admin/teams/{id}/plan`). A team that already has a subscription row is left alone.
const UP: &str = r#"
INSERT INTO subscriptions (team_id, plan_id, status, provider)
SELECT id, 'automation', 'active', 'comp' FROM teams
ON CONFLICT (team_id) DO NOTHING;"#;

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
