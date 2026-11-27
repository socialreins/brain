use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// `team_slug_holds` was replaced by `team_slug_history` (m20270103), which copied its rows and left
// the table "for one release" so an instance running during that deploy would not fail on it.
// Nothing in brain or web reads or writes it any more (grep of both repos), and its rows were
// copied then.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection()
            .execute_unprepared("DROP TABLE IF EXISTS team_slug_holds")
            .await
            .map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "CREATE TABLE IF NOT EXISTS team_slug_holds (
    slug text PRIMARY KEY,
    team_id uuid NOT NULL,
    released_at timestamptz NOT NULL DEFAULT now()
)";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
