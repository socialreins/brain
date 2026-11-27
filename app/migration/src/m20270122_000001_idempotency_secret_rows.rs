use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Before `idempotency::stores`, the response of every write was kept, so an `Idempotency-Key` on
// `POST /api-tokens` or `PUT /team/webhook` stored the secret those routes answer once (a bearer
// token, a signing secret) for 30 days, readable by every token of the team through
// `GET /v1/idempotency/{key}`. The layer no longer stores either; this deletes the old rows, so a
// key sent in the last 30 days cannot hand a secret out. The rows are worthless without the
// answer: a retry runs the route again, and a secret already answered once is the caller's.
const UP: &str = "DELETE FROM idempotency_keys \
                  WHERE path IN ('/api-tokens', '/team/webhook') \
                  AND status = 'done' AND response_body IS NOT NULL;";

const DOWN: &str = "SELECT 1;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection()
            .execute_unprepared(DOWN)
            .await
            .map(|_| ())
    }
}
