use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// `teams.code`: the team's address in the web app (`/t/<code>/...`). Six characters from
// `socialreins_core::team::CODE_ALPHABET` (no 0, o, 1, l, i), lowercase, unique, never changed.
// Existing teams get a random one here, drawn again on the rare clash; new teams get theirs
// from the app.
/// Gives every team without a code a random one. Idempotent.
pub const BACKFILL: &str = "DO $$
DECLARE t uuid; c text;
BEGIN
  FOR t IN SELECT id FROM teams WHERE code IS NULL LOOP
    LOOP
      SELECT string_agg(substr('23456789abcdefghjkmnpqrstuvwxyz', 1 + floor(random() * 31)::int, 1), '')
        INTO c FROM generate_series(1, 6);
      BEGIN
        UPDATE teams SET code = c WHERE id = t;
        EXIT;
      EXCEPTION WHEN unique_violation THEN
      END;
    END LOOP;
  END LOOP;
END $$;";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let db = m.get_connection();
        db.execute_unprepared("ALTER TABLE teams ADD COLUMN code text UNIQUE;")
            .await?;
        db.execute_unprepared(BACKFILL).await?;
        let sql = "ALTER TABLE teams ALTER COLUMN code SET NOT NULL;
ALTER TABLE teams ADD CONSTRAINT teams_code_format CHECK (code ~ '^[2-9a-hjkmnp-z]{6}$');";
        db.execute_unprepared(sql).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let sql = "ALTER TABLE teams DROP COLUMN IF EXISTS code;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
