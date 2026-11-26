use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

// Indexes the scaling and performance review found missing. One transaction like every other
// migration (brain's boot holds an advisory lock in a transaction while it migrates, so
// `CREATE INDEX CONCURRENTLY`, which waits for every open transaction, would wait for that one
// forever). Each build blocks writes to its table while it runs: 0.3 s for `inbox_items` at
// 400,000 rows, milliseconds for the rest. `lock_timeout` fails this boot instead of queueing
// every writer behind a build that is itself waiting.
// - `inbox_items_open_recent_idx`: the open list and the open conversations, newest first.
// - `inbox_items_post_idx`: deleting a post sets `inbox_items.post_id` to null, which without an
//   index reads every inbox item of every team.
// - `social_accounts_provider_external_idx`, `social_accounts_threads_handle_idx`: `loop_guard`
//   asks which team owns an account by provider and external id, and a Threads one by handle.
// - `post_targets_account_remote_idx`: an inbox item finds the post it answers by the account and
//   the remote id of a target.
const INDEXES: [(&str, &str); 5] = [
    (
        "inbox_items_open_recent_idx",
        "inbox_items (team_id, received_at DESC, id DESC) WHERE state = 'open'",
    ),
    (
        "inbox_items_post_idx",
        "inbox_items (post_id) WHERE post_id IS NOT NULL",
    ),
    (
        "social_accounts_provider_external_idx",
        "social_accounts (provider, external_id)",
    ),
    (
        "social_accounts_threads_handle_idx",
        "social_accounts (lower(handle)) WHERE provider = 'threads'",
    ),
    (
        "post_targets_account_remote_idx",
        "post_targets (social_account_id, remote_id)",
    ),
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let mut sql = String::from("SET LOCAL lock_timeout = '10s';\n");
        for (name, on) in INDEXES {
            sql += &format!("CREATE INDEX {name} ON {on};\n");
        }
        m.get_connection().execute_unprepared(&sql).await.map(|_| ())
    }

    async fn down(&self, m: &SchemaManager) -> Result<(), DbErr> {
        let mut sql = String::new();
        for (name, _) in INDEXES {
            sql += &format!("DROP INDEX IF EXISTS {name};\n");
        }
        m.get_connection().execute_unprepared(&sql).await.map(|_| ())
    }
}
