use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// The team's own logo above its newsletters: a public `https` image link, optional.
// `teams.newsletter_sender_logo` is set with the sender details, and a start copies it onto
// the newsletter (`newsletters.sender_logo`) as it does the name and address. brain never
// fetches it: it is only the `src` of an image in the mail.
const UP: &str = "ALTER TABLE teams ADD COLUMN newsletter_sender_logo text;
ALTER TABLE newsletters ADD COLUMN sender_logo text;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE newsletters DROP COLUMN IF EXISTS sender_logo;
ALTER TABLE teams DROP COLUMN IF EXISTS newsletter_sender_logo;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
