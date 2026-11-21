use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// `stripe_events`: every Stripe event id taken, so a redelivery changes nothing. The id is
// written in the same transaction as the change, so a failure leaves it unwritten and Stripe's
// retry runs the event again. `provider_event_at` on a subscription is the `created` time of the
// last event applied to it: an older event arriving later (Stripe does not keep order) is dropped.
const UP: &str = r#"
CREATE TABLE stripe_events (
  id text PRIMARY KEY,
  type text NOT NULL,
  received_at timestamptz NOT NULL DEFAULT now());
ALTER TABLE subscriptions ADD COLUMN provider_event_at timestamptz;"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE subscriptions DROP COLUMN IF EXISTS provider_event_at;
                   DROP TABLE IF EXISTS stripe_events";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
