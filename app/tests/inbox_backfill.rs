//! The `inbox_accounts` migration on old-shape inboxes, in a database of its own made from
//! `DATABASE_URL` (the user must be able to create databases, as in CI).
use std::collections::{BTreeMap, BTreeSet};

use migration::{Migrator, MigratorTrait};
use sea_orm::{ConnectionTrait, Database, DatabaseConnection};
use serde_json::{json, Value};

const NAME: &str = "m20261217_000001_inbox_accounts";

/// Each inbox, by "team/name", with its accounts (by external id).
async fn held(db: &DatabaseConnection) -> BTreeMap<String, BTreeSet<String>> {
    let rows = db
        .query_all_raw(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Postgres,
            "SELECT t.name || '/' || b.name AS inbox, a.external_id FROM inboxes b \
             JOIN teams t ON t.id = b.team_id \
             LEFT JOIN inbox_accounts ia ON ia.inbox_id = b.id \
             LEFT JOIN social_accounts a ON a.id = ia.social_account_id",
        ))
        .await
        .unwrap();
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for r in rows {
        let set = out.entry(r.try_get("", "inbox").unwrap()).or_default();
        if let Some(a) = r.try_get::<Option<String>>("", "external_id").unwrap() {
            set.insert(a);
        }
    }
    out
}

async fn filters(db: &DatabaseConnection) -> BTreeMap<String, Value> {
    let rows = db
        .query_all_raw(sea_orm::Statement::from_string(
            sea_orm::DbBackend::Postgres,
            "SELECT t.name || '/' || b.name AS inbox, b.filters FROM inboxes b \
             JOIN teams t ON t.id = b.team_id",
        ))
        .await
        .unwrap();
    rows.into_iter()
        .map(|r| {
            (
                r.try_get("", "inbox").unwrap(),
                r.try_get("", "filters").unwrap(),
            )
        })
        .collect()
}

#[tokio::test]
async fn backfill_keeps_each_old_scope_and_down_restores_it() {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
    let (server, _) = url.rsplit_once('/').unwrap();
    let name = format!("backfill_{}", uuid::Uuid::now_v7().simple());
    let admin = Database::connect(&url).await.unwrap();
    admin
        .execute_unprepared(&format!("CREATE DATABASE {name}"))
        .await
        .unwrap();
    let db = Database::connect(format!("{server}/{name}")).await.unwrap();

    let before = Migrator::migrations()
        .iter()
        .position(|m| m.name() == NAME)
        .unwrap();
    Migrator::up(&db, Some(before as u32)).await.unwrap();

    // t1: A1 bluesky, A2 facebook, A3 instagram, A4 x (no inbox). Set s holds A1 and A2; the
    // deleted set d holds A1. t2 has no custom inbox; t3 has no accounts either.
    db.execute_unprepared(
        "INSERT INTO teams (id, name, code) VALUES \
           ('00000000-0000-0000-0000-0000000000a1', 't1', 'aaaaa2'), \
           ('00000000-0000-0000-0000-0000000000a2', 't2', 'aaaaa3'), \
           ('00000000-0000-0000-0000-0000000000a3', 't3', 'aaaaa4'); \
         INSERT INTO social_accounts (id, team_id, provider, external_id, access_token) VALUES \
           ('00000000-0000-0000-0000-0000000000b1', '00000000-0000-0000-0000-0000000000a1', 'bluesky', 'A1', '\\x00'), \
           ('00000000-0000-0000-0000-0000000000b2', '00000000-0000-0000-0000-0000000000a1', 'facebook', 'A2', '\\x00'), \
           ('00000000-0000-0000-0000-0000000000b3', '00000000-0000-0000-0000-0000000000a1', 'instagram', 'A3', '\\x00'), \
           ('00000000-0000-0000-0000-0000000000b4', '00000000-0000-0000-0000-0000000000a1', 'x', 'A4', '\\x00'), \
           ('00000000-0000-0000-0000-0000000000c1', '00000000-0000-0000-0000-0000000000a2', 'bluesky', 'B1', '\\x00'), \
           ('00000000-0000-0000-0000-0000000000c2', '00000000-0000-0000-0000-0000000000a2', 'x', 'B2', '\\x00'); \
         INSERT INTO social_sets (id, team_id, name, deleted_at) VALUES \
           ('00000000-0000-0000-0000-0000000000d1', '00000000-0000-0000-0000-0000000000a1', 's', NULL), \
           ('00000000-0000-0000-0000-0000000000d2', '00000000-0000-0000-0000-0000000000a1', 'd', now()); \
         INSERT INTO social_account_social_set (social_account_id, social_set_id) VALUES \
           ('00000000-0000-0000-0000-0000000000b1', '00000000-0000-0000-0000-0000000000d1'), \
           ('00000000-0000-0000-0000-0000000000b2', '00000000-0000-0000-0000-0000000000d1'), \
           ('00000000-0000-0000-0000-0000000000b1', '00000000-0000-0000-0000-0000000000d2'); \
         INSERT INTO inboxes (id, team_id, name, is_all, filters) VALUES \
           (gen_random_uuid(), '00000000-0000-0000-0000-0000000000a1', 'All', true, '{}'), \
           (gen_random_uuid(), '00000000-0000-0000-0000-0000000000a1', 'set', false, \
             '{\"social_set_id\": \"00000000-0000-0000-0000-0000000000d1\"}'), \
           (gen_random_uuid(), '00000000-0000-0000-0000-0000000000a1', 'ids', false, \
             '{\"account_ids\": [\"00000000-0000-0000-0000-0000000000b1\", \"00000000-0000-0000-0000-0000000000b3\"]}'), \
           (gen_random_uuid(), '00000000-0000-0000-0000-0000000000a1', 'both', false, \
             '{\"social_set_id\": \"00000000-0000-0000-0000-0000000000d1\", \
               \"account_ids\": [\"00000000-0000-0000-0000-0000000000b2\", \"00000000-0000-0000-0000-0000000000b3\"]}'), \
           (gen_random_uuid(), '00000000-0000-0000-0000-0000000000a1', 'platforms', false, \
             '{\"platforms\": [\"instagram\", \"x\"], \"kinds\": [\"dm\"]}'), \
           (gen_random_uuid(), '00000000-0000-0000-0000-0000000000a1', 'deleted', false, \
             '{\"social_set_id\": \"00000000-0000-0000-0000-0000000000d2\"}'), \
           (gen_random_uuid(), '00000000-0000-0000-0000-0000000000a1', 'any', false, '{}'), \
           (gen_random_uuid(), '00000000-0000-0000-0000-0000000000a2', 'All', true, '{}');",
    )
    .await
    .unwrap();
    let old = filters(&db).await;

    Migrator::up(&db, Some(1)).await.unwrap();
    let set = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    let want: BTreeMap<String, BTreeSet<String>> = [
        ("t1/All", set(&[])),
        ("t1/set", set(&["A1", "A2"])),
        ("t1/ids", set(&["A1", "A3"])),
        ("t1/both", set(&["A2"])),
        ("t1/platforms", set(&["A3"])),
        ("t1/deleted", set(&[])),
        ("t1/any", set(&["A1", "A2", "A3"])),
        ("t2/All", set(&[])),
        ("t2/Inbox", set(&["B1"])),
        ("t3/Inbox", set(&[])),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    assert_eq!(held(&db).await, want);
    let now = filters(&db).await;
    assert_eq!(now["t1/set"], json!({}));
    assert_eq!(now["t1/both"], json!({}));
    assert_eq!(now["t1/platforms"], json!({"kinds": ["dm"]}));

    // A later edit of the other filters survives a rollback; the old scope comes back.
    db.execute_unprepared(
        "UPDATE inboxes SET filters = '{\"kinds\": [\"comment\"]}' WHERE name = 'platforms'",
    )
    .await
    .unwrap();
    Migrator::down(&db, Some(1)).await.unwrap();
    let mut back = filters(&db).await;
    assert_eq!(
        back.remove("t1/platforms").unwrap(),
        json!({"platforms": ["instagram", "x"], "kinds": ["comment"]})
    );
    assert_eq!(back.remove("t2/Inbox").unwrap(), json!({}));
    assert_eq!(back.remove("t3/Inbox").unwrap(), json!({}));
    let mut old = old;
    old.remove("t1/platforms");
    assert_eq!(back, old);

    db.close().await.unwrap();
    admin
        .execute_unprepared(&format!("DROP DATABASE {name}"))
        .await
        .unwrap();
}
