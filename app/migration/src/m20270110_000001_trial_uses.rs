use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// One free trial per person, not per team (decision 0015): a row per person key (`email::person_key`)
// whose trial began. No foreign key: the record must outlive the team and the user. Teams that
// already had a trial are not backfilled: billing is not on in production, so none exist.
const UP: &str = r#"CREATE TABLE trial_uses (
  person_key text PRIMARY KEY,
  team_id uuid,
  used_at timestamptz NOT NULL DEFAULT now()
)"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection()
            .execute_unprepared("DROP TABLE trial_uses")
            .await
            .map(|_| ())
    }
}
