use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// The rest of the `ILIKE '%q%'` searches (scaling review, finding 4). #204 gave the inbox one
// trigram index per searched column; the contacts, media library and admin lists search the same
// way over their whole table with no index, so each keystroke reads every row of the team's (or
// every team's) table:
// - `contacts_sql` (`controllers::contacts`, the list and the automations picker) matches
//   `handle` or `display_name`;
// - `list_media` (`controllers::library`) matches `name` or `url`;
// - the admin users and teams lists match `email`, `name` or `teams.name` (`admin.rs` had a
//   "shortcut" comment asking for this).
//
// `gin_trgm_ops` answers `LIKE` and `ILIKE` on the plain column, so no query changes and no
// `lower()` expression indexes: the same statements, the same rows, and the planner finds the
// index (a generic plan does too, without `plan_cache_mode`). `pg_trgm` is already there since
// #204 (the `CREATE EXTENSION IF NOT EXISTS` is a no-op here and keeps this migration readable
// on its own).
//
// Measured on a local Postgres 17.12 with generated data (100,000 contacts over 10 teams,
// 50,000 media library rows over 10 teams, 20,000 users and 5,000 teams), warm cache, a rare
// term searched on one 10,000-contact team: contacts 27.5 ms to 1.3 ms, media library 7.1 ms to
// 1.2 ms, admin users 15.4 ms to 0.19 ms, admin teams 1.9 ms to 0.06 ms; a term matching
// nothing in the team, 11.3 ms to 0.02 ms. A broad term stays on the team's own index (0.2 ms,
// unchanged). All seven indexes build in 1.1 s together and take 14 MB at that size. One
// transaction like every other migration (brain's boot holds an
// advisory lock in a transaction while it migrates, so `CREATE INDEX CONCURRENTLY` would wait
// for it forever). Each build blocks writes to its table: milliseconds at production's size.
// `lock_timeout` fails this boot instead of queueing every writer behind a build that is itself
// waiting. `down` drops the indexes and leaves the extension: it is not ours alone.
const INDEXES: [(&str, &str, &str); 7] = [
    ("contacts_handle_trgm_idx", "contacts", "handle"),
    ("contacts_display_name_trgm_idx", "contacts", "display_name"),
    ("media_library_name_trgm_idx", "media_library", "name"),
    ("media_library_url_trgm_idx", "media_library", "url"),
    ("users_email_trgm_idx", "users", "email"),
    ("users_name_trgm_idx", "users", "name"),
    ("teams_name_trgm_idx", "teams", "name"),
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let mut sql = String::from(
            "SET LOCAL lock_timeout = '10s';\nCREATE EXTENSION IF NOT EXISTS pg_trgm;\n",
        );
        for (name, table, column) in INDEXES {
            sql += &format!("CREATE INDEX {name} ON {table} USING gin ({column} gin_trgm_ops);\n");
        }
        m.get_connection()
            .execute_unprepared(&sql)
            .await
            .map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let mut sql = String::new();
        for (name, _, _) in INDEXES {
            sql += &format!("DROP INDEX IF EXISTS {name};\n");
        }
        m.get_connection()
            .execute_unprepared(&sql)
            .await
            .map(|_| ())
    }
}
