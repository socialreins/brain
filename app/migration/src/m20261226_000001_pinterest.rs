use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Pinterest: per-post settings (`posts.pinterest`, null for none: the board, title and link) and
// the platform in the check on `posts.platform`. The check is rebuilt from what it holds now plus
// `pinterest`, so a platform another migration added is kept.
const UP: &str = r#"ALTER TABLE posts ADD COLUMN pinterest jsonb;
DO $$
DECLARE def text;
BEGIN
  SELECT pg_get_constraintdef(oid) INTO def FROM pg_constraint
    WHERE conrelid = 'posts'::regclass AND conname = 'posts_platform_check';
  IF def IS NOT NULL AND def NOT LIKE '%''pinterest''%' THEN
    ALTER TABLE posts DROP CONSTRAINT posts_platform_check;
    EXECUTE 'ALTER TABLE posts ADD CONSTRAINT posts_platform_check '
      || replace(def, 'ARRAY[', 'ARRAY[''pinterest''::text, ');
  END IF;
END $$;"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        m.get_connection().execute_unprepared(UP).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        // The platform stays in the check: rows may hold it.
        let sql = "ALTER TABLE posts DROP COLUMN IF EXISTS pinterest;";
        m.get_connection().execute_unprepared(sql).await.map(|_| ())
    }
}
