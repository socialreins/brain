use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// A DM conversation is every `dm` item of one account and contact: this index reads one, newest
// first, and finds the items a change of the whole conversation reaches.
const UP: &str = "CREATE INDEX inbox_items_conversation_idx ON inbox_items \
    (team_id, social_account_id, contact_id, received_at DESC, id DESC) WHERE kind = 'dm'";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "DROP INDEX IF EXISTS inbox_items_conversation_idx";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
