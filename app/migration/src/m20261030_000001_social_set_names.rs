use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Live set names are unique per team in any case; deleted sets do not hold their name.
const UP: &str = "CREATE UNIQUE INDEX social_sets_live_name_idx \
                  ON social_sets (team_id, lower(name)) WHERE deleted_at IS NULL";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP INDEX IF EXISTS social_sets_live_name_idx";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
