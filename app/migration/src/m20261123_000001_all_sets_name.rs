use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// The set that holds every account is now named "All sets". Only a default set still named exactly
// "Default" is renamed: one a user renamed keeps its name, and so does a team that already has
// another live set named "All sets" (live names are unique per team in any case).
pub const UP: &str = "UPDATE social_sets s SET name = 'All sets' \
    WHERE s.is_default AND s.name = 'Default' AND NOT EXISTS ( \
        SELECT 1 FROM social_sets o WHERE o.team_id = s.team_id AND o.id <> s.id \
        AND o.deleted_at IS NULL AND lower(o.name) = 'all sets')";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    // Nothing to undo: a renamed set cannot be told from one a user named "All sets".
    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
