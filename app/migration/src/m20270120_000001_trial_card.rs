use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// One free trial per card as well as per person (security review): the webhook reads the
// subscription's payment method from Stripe when a trial starts and stores the SHA-256 of its
// card fingerprint here. A card that already trialed, under any address, is not offered another.
// Nullable because a trial may collect no card at all (Stripe has a payment method only when
// checkout collected one): such a trial is recorded by `person_key` alone, as before.
const UP: &str = r#"ALTER TABLE trial_uses ADD COLUMN card_hash text;
CREATE INDEX trial_uses_card_hash_idx ON trial_uses (card_hash)"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection()
            .execute_unprepared(
                "DROP INDEX trial_uses_card_hash_idx; ALTER TABLE trial_uses DROP COLUMN card_hash",
            )
            .await
            .map(|_| ())
    }
}
