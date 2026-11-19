use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Post approvals: `teams.require_approval` holds a member's scheduled posts back until an owner
// or admin approves them. `posts.approval` is null (none asked), `pending`, `approved` or
// `rejected`; the publish tick reads it only while the team's switch is on.
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE teams ADD COLUMN require_approval boolean NOT NULL DEFAULT false;
ALTER TABLE posts
    ADD COLUMN approval text CHECK (approval IN ('pending', 'approved', 'rejected')),
    ADD COLUMN approval_note text,
    ADD COLUMN approval_by uuid REFERENCES users (id) ON DELETE SET NULL,
    ADD COLUMN approval_at timestamptz;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE posts DROP COLUMN IF EXISTS approval,
    DROP COLUMN IF EXISTS approval_note, DROP COLUMN IF EXISTS approval_by,
    DROP COLUMN IF EXISTS approval_at;
ALTER TABLE teams DROP COLUMN IF EXISTS require_approval;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
