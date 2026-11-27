use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Inbox search is `ILIKE '%q%'` over `text`, `author_name` and `author_handle` (scaling review,
// finding 4): without a trigram index every search reads the team's whole inbox (683 ms to 795 ms
// at 400,000 items, measured on a local Postgres 18 with generated data). One GIN index on each
// column lets Postgres find a rare term from the index (0.4 ms for the same data). Custom inbox
// keyword filters use the same `ILIKE` on `text`.
//
// `pg_trgm` ships with Postgres's contrib set (Railway's Postgres image, the official Docker
// image, `postgres:17` in CI) and is a trusted extension, so a database owner may create it.
// One transaction like every other migration (brain's boot holds an advisory lock in a
// transaction while it migrates, so `CREATE INDEX CONCURRENTLY` would wait for it forever). Each
// build blocks writes to `inbox_items` while it runs: 2.7 s for all three at 400,000 items,
// milliseconds at production's size. `lock_timeout` fails this boot instead of queueing every
// writer behind a build that is itself waiting. `down` leaves the extension: it is not ours alone.
const INDEXES: [(&str, &str); 3] = [
    ("inbox_items_text_trgm_idx", "text"),
    ("inbox_items_author_name_trgm_idx", "author_name"),
    ("inbox_items_author_handle_trgm_idx", "author_handle"),
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let mut sql = String::from(
            "SET LOCAL lock_timeout = '10s';\nCREATE EXTENSION IF NOT EXISTS pg_trgm;\n",
        );
        for (name, column) in INDEXES {
            sql +=
                &format!("CREATE INDEX {name} ON inbox_items USING gin ({column} gin_trgm_ops);\n");
        }
        m.get_connection()
            .execute_unprepared(&sql)
            .await
            .map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let mut sql = String::new();
        for (name, _) in INDEXES {
            sql += &format!("DROP INDEX IF EXISTS {name};\n");
        }
        m.get_connection()
            .execute_unprepared(&sql)
            .await
            .map(|_| ())
    }
}
