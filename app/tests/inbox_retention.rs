//! Inbox retention: the daily purge deletes items older than the window with their replies and
//! automation runs, in batches, and leaves contacts, their tags, posts and media alone. Needs
//! `DATABASE_URL` like the request tests.
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, TransactionTrait};
use serial_test::serial;
use socialreins_brain::{
    config::Config,
    connect,
    scheduler::{purge_old_inbox_items, INBOX_RETENTION_LOCK},
};
use uuid::Uuid;

struct Env {
    db: DatabaseConnection,
    team: Uuid,
    account: Uuid,
    contact: Uuid,
    automation: Uuid,
}

async fn exec(db: &DatabaseConnection, sql: &str) {
    db.execute_unprepared(sql).await.unwrap();
}

/// A team with an account, a contact (with one tag), a media library entry, a post and an
/// automation, ready for items of any age.
async fn env() -> Env {
    let config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    let db = connect(&config).await.expect("database reachable");
    let (team, set, account, contact, automation, post) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    // Six characters of `socialreins_core::team::CODE_ALPHABET`, as `teams.code` requires.
    const ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";
    let mut n = team.as_u128();
    let code: String = (0..6)
        .map(|_| {
            let c = ALPHABET[(n % ALPHABET.len() as u128) as usize];
            n /= ALPHABET.len() as u128;
            c as char
        })
        .collect();
    exec(
        &db,
        &format!(
            "INSERT INTO teams (id, name, code) VALUES ('{team}', 'retention', '{code}'); \
             INSERT INTO social_sets (id, team_id, name) VALUES ('{set}', '{team}', 's'); \
             INSERT INTO social_accounts (id, team_id, provider, external_id, access_token) \
               VALUES ('{account}', '{team}', 'bluesky', 'did:plc:{account}', decode('00', 'hex')); \
             INSERT INTO contacts (id, team_id, platform, external_id, first_seen_at, last_seen_at) \
               VALUES ('{contact}', '{team}', 'bluesky', 'did:plc:fan', now(), now()); \
             INSERT INTO contact_tags (contact_id, tag) VALUES ('{contact}', 'vip'); \
             INSERT INTO media_library (id, team_id, url, name) \
               VALUES (gen_random_uuid(), '{team}', 'https://example.com/a.png', 'a'); \
             INSERT INTO automations (id, team_id, name, trigger, actions) \
               VALUES ('{automation}', '{team}', 'a', 'comment_received', '[]'); \
             INSERT INTO posts (id, team_id, social_set_id, platform, status) \
               VALUES ('{post}', '{team}', '{set}', 'bluesky', 'draft')"
        ),
    )
    .await;
    Env {
        db,
        team,
        account,
        contact,
        automation,
    }
}

impl Env {
    /// Removes old items a test before this one left behind, so a count of what the purge
    /// deleted is this test's own (the job is global, over every team).
    async fn clear(&self) {
        purge_old_inbox_items(&self.db, 365, 1000).await.unwrap();
    }

    /// An item `age` old with a public reply, an automation run and a miss. Returns its id.
    async fn item(&self, tag: &str, age: &str) -> Uuid {
        let item = Uuid::now_v7();
        let (team, account, contact, automation) =
            (self.team, self.account, self.contact, self.automation);
        exec(
            &self.db,
            &format!(
                "INSERT INTO inbox_items (id, team_id, social_account_id, platform, kind, \
                   remote_id, author_id, text, received_at, created_at, contact_id) \
                 VALUES ('{item}', '{team}', '{account}', 'bluesky', 'comment', 'r-{tag}', \
                   'did:plc:fan', 'hi', now() - interval '{age}', now() - interval '{age}', \
                   '{contact}'); \
                 INSERT INTO inbox_replies (id, inbox_item_id, kind, text) \
                   VALUES (gen_random_uuid(), '{item}', 'public', 'hello'); \
                 INSERT INTO automation_runs (id, automation_id, inbox_item_id, team_id, status) \
                   VALUES (gen_random_uuid(), '{automation}', '{item}', '{team}', 'sent'); \
                 INSERT INTO automation_misses (automation_id, inbox_item_id, team_id, reasons) \
                   VALUES ('{automation}', '{item}', '{team}', ARRAY['no_keyword'])"
            ),
        )
        .await;
        item
    }

    /// How many rows `sql` (one `count(*) AS count`) finds.
    async fn count(&self, sql: &str) -> i64 {
        let row = self
            .db
            .query_one_raw(Statement::from_string(DbBackend::Postgres, sql.to_owned()))
            .await
            .unwrap()
            .expect("a count row");
        row.try_get("", "count").unwrap()
    }

    async fn has(&self, table: &str, id: Uuid) -> i64 {
        self.count(&format!(
            "SELECT count(*) AS count FROM {table} WHERE id = '{id}'"
        ))
        .await
    }
}

#[tokio::test]
#[serial]
async fn an_item_past_the_window_goes_with_its_replies_and_runs_and_the_rest_stays() {
    let e = env().await;
    e.clear().await;
    let old = e.item("old", "400 days").await;
    let kept = e.item("kept", "300 days").await;
    assert_eq!(
        purge_old_inbox_items(&e.db, 365, 1000).await.unwrap(),
        1,
        "only the item past the window"
    );
    assert_eq!(e.has("inbox_items", old).await, 0);
    assert_eq!(e.has("inbox_items", kept).await, 1);
    assert_eq!(
        e.count(&format!(
            "SELECT count(*) AS count FROM inbox_replies WHERE inbox_item_id = '{old}'"
        ))
        .await,
        0,
        "the old item's reply went with it"
    );
    assert_eq!(
        e.count(&format!(
            "SELECT count(*) AS count FROM automation_runs WHERE inbox_item_id = '{old}'"
        ))
        .await,
        0
    );
    assert_eq!(
        e.count(&format!(
            "SELECT count(*) AS count FROM automation_misses WHERE inbox_item_id = '{old}'"
        ))
        .await,
        0
    );
    assert_eq!(
        e.count(&format!(
            "SELECT count(*) AS count FROM inbox_replies WHERE inbox_item_id = '{kept}'"
        ))
        .await,
        1,
        "the kept item keeps its reply"
    );
    // Nothing else goes with it.
    assert_eq!(e.has("contacts", e.contact).await, 1);
    assert_eq!(
        e.count(&format!(
            "SELECT count(*) AS count FROM contact_tags WHERE contact_id = '{}'",
            e.contact
        ))
        .await,
        1
    );
    assert_eq!(
        e.count(&format!(
            "SELECT count(*) AS count FROM media_library WHERE team_id = '{}'",
            e.team
        ))
        .await,
        1
    );
    assert_eq!(
        e.count(&format!(
            "SELECT count(*) AS count FROM posts WHERE team_id = '{}'",
            e.team
        ))
        .await,
        1
    );
    // A second daily run finds nothing left.
    assert_eq!(purge_old_inbox_items(&e.db, 365, 1000).await.unwrap(), 0);
}

#[tokio::test]
#[serial]
async fn zero_days_keeps_every_item() {
    let e = env().await;
    e.clear().await;
    let old = e.item("very-old", "4000 days").await;
    assert_eq!(purge_old_inbox_items(&e.db, 0, 1000).await.unwrap(), 0);
    assert_eq!(e.has("inbox_items", old).await, 1);
}

#[tokio::test]
#[serial]
async fn batching_deletes_more_than_one_batch() {
    let e = env().await;
    e.clear().await;
    for tag in ["a", "b", "c", "d", "e"] {
        e.item(tag, "400 days").await;
    }
    let recent = e.item("recent", "300 days").await;
    // Two rows a statement: five old items take three rounds.
    assert_eq!(purge_old_inbox_items(&e.db, 365, 2).await.unwrap(), 5);
    assert_eq!(
        e.count(&format!(
            "SELECT count(*) AS count FROM inbox_items WHERE team_id = '{}' AND id <> '{recent}'",
            e.team
        ))
        .await,
        0
    );
    assert_eq!(e.has("inbox_items", recent).await, 1);
}

#[tokio::test]
#[serial]
async fn a_round_skips_while_another_instance_holds_the_lock() {
    let e = env().await;
    e.clear().await;
    let old = e.item("old", "400 days").await;
    // Another instance in the middle of its round holds the lock until its transaction ends.
    let txn = e.db.begin().await.unwrap();
    txn.execute_unprepared(&format!(
        "SELECT pg_advisory_xact_lock({INBOX_RETENTION_LOCK})"
    ))
    .await
    .unwrap();
    assert_eq!(
        purge_old_inbox_items(&e.db, 365, 1000).await.unwrap(),
        0,
        "the round ran while another instance held the lock"
    );
    assert_eq!(
        e.has("inbox_items", old).await,
        1,
        "an item was deleted anyway"
    );
    txn.rollback().await.unwrap();
    assert_eq!(
        purge_old_inbox_items(&e.db, 365, 1000).await.unwrap(),
        1,
        "the rest waits for the next round"
    );
}

#[tokio::test]
#[serial]
async fn two_runs_at_once_delete_each_item_once() {
    let e = env().await;
    e.clear().await;
    let mut items = Vec::new();
    for tag in ["a", "b", "c", "d", "e", "f"] {
        items.push(e.item(tag, "400 days").await);
    }
    // One row a statement, both runs at once: the lock hands each round to one of them.
    let (first, second) = tokio::join!(
        purge_old_inbox_items(&e.db, 365, 1),
        purge_old_inbox_items(&e.db, 365, 1)
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(
        first + second,
        6,
        "a row deleted twice: {first} and {second}"
    );
    for item in items {
        assert_eq!(e.has("inbox_items", item).await, 0);
    }
}
