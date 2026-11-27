use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// A live process's hold on a team's publishing place (decision 0009). `claim` sets it to two
// minutes ahead and the process running the post renews it every 30 seconds, so a team's cap
// holds across replicas and a crashed process's places free once the lease runs out. The partial
// index serves the count of a team's live leases (`publish::claim`).
const UP: &str = r#"ALTER TABLE posts ADD COLUMN lease_until timestamptz;
CREATE INDEX posts_lease_idx ON posts (team_id) WHERE status = 'queued';"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP INDEX IF EXISTS posts_lease_idx; \
                   ALTER TABLE posts DROP COLUMN IF EXISTS lease_until";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
