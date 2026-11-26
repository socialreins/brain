//! The Bluesky and Meta inbox ticks' claims: accounts are taken oldest read first, stamped so
//! they are not due again at once, a paused account is left alone, and one tick reaches every
//! account that is due, not a few. Needs `DATABASE_URL` like the request
//! tests. The accounts hold no usable password, so nothing is read from a network.
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use serial_test::serial;
use socialreins_brain::{config::Config, connect, inbox, meta_inbox, state::AppState};
use uuid::Uuid;

#[tokio::test]
#[serial]
async fn the_tick_stamps_what_it_claims_and_skips_what_is_paused_or_fresh() {
    let config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    let db = connect(&config).await.expect("database reachable");
    // Rows other runs left would be claimed too: park them.
    db.execute_unprepared(
        "UPDATE social_accounts SET inbox_read_at = now() WHERE provider = 'bluesky'",
    )
    .await
    .unwrap();
    let state = AppState::new(db.clone(), &config);
    let team = Uuid::now_v7();
    db.execute_unprepared(&format!(
        "INSERT INTO teams (id, name, code) VALUES ('{team}', 't', '{}')",
        // Six letters of the code alphabet, from the id's digits.
        team.as_u128()
            .to_string()
            .bytes()
            .rev()
            .take(6)
            .map(|d| (b'p' + d % 10) as char)
            .collect::<String>()
    ))
    .await
    .unwrap();
    // Names: never read, read long ago, read just now, paused for a refused password.
    for (name, read, error) in [
        ("never", "NULL", "NULL"),
        ("old", "now() - interval '10 minutes'", "NULL"),
        ("fresh", "now() - interval '5 seconds'", "NULL"),
        ("paused", "NULL", "now() - interval '5 minutes'"),
    ] {
        db.execute_unprepared(&format!(
            "INSERT INTO social_accounts (id, team_id, provider, external_id, access_token, \
             inbox_read_at, inbox_error_at) VALUES ('{}', '{team}', 'bluesky', '{name}', \
             '\\x00', {read}, {error})",
            Uuid::now_v7()
        ))
        .await
        .unwrap();
    }
    // More due accounts than one chunk, so the tick has to claim again.
    for i in 0..60 {
        db.execute_unprepared(&format!(
            "INSERT INTO social_accounts (id, team_id, provider, external_id, access_token) \
             VALUES ('{}', '{team}', 'bluesky', 'many{i}', '\\x00')",
            Uuid::now_v7()
        ))
        .await
        .unwrap();
    }
    inbox::tick(&state).await.unwrap();
    let r = db
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            format!(
                "SELECT count(*) FILTER (WHERE inbox_read_at > now() - interval '30 seconds') AS n \
                 FROM social_accounts WHERE team_id = '{team}' AND external_id LIKE 'many%'"
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.try_get::<i64>("", "n").unwrap(), 60);
    let rows = db
        .query_all_raw(Statement::from_string(
            DbBackend::Postgres,
            format!(
                "SELECT external_id, inbox_read_at > now() - interval '2 seconds' AS claimed, \
                 inbox_read_at IS NULL AS never FROM social_accounts WHERE team_id = '{team}' AND external_id NOT LIKE 'many%'"
            ),
        ))
        .await
        .unwrap();
    let mut got: Vec<(String, bool, bool)> = rows
        .iter()
        .map(|r| {
            (
                r.try_get("", "external_id").unwrap(),
                r.try_get::<Option<bool>>("", "claimed")
                    .unwrap()
                    .unwrap_or(false),
                r.try_get("", "never").unwrap(),
            )
        })
        .collect();
    got.sort();
    // "fresh" keeps its own 5 second old stamp, "paused" was not claimed.
    assert_eq!(
        got,
        [
            ("fresh".into(), false, false),
            ("never".into(), true, false),
            ("old".into(), true, false),
            ("paused".into(), false, true),
        ]
    );
}

#[tokio::test]
#[serial]
async fn one_meta_backfill_tick_claims_every_account_that_is_due() {
    let config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    let db = connect(&config).await.expect("database reachable");
    db.execute_unprepared(
        "UPDATE social_accounts SET meta_synced_at = now() WHERE provider IN ('facebook', 'instagram')",
    )
    .await
    .unwrap();
    let state = AppState::new(db.clone(), &config);
    let team = Uuid::now_v7();
    db.execute_unprepared(&format!(
        "INSERT INTO teams (id, name, code) VALUES ('{team}', 't', '{}')",
        team.as_u128()
            .to_string()
            .bytes()
            .rev()
            .take(6)
            .map(|d| (b'p' + d % 10) as char)
            .collect::<String>()
    ))
    .await
    .unwrap();
    // 40 due accounts (the old tick took 4), plus one read a minute ago that is not due.
    for i in 0..41 {
        let read = if i == 40 {
            "now() - interval '1 minute'"
        } else {
            "NULL"
        };
        db.execute_unprepared(&format!(
            "INSERT INTO social_accounts (id, team_id, provider, external_id, access_token, \
             meta_synced_at) VALUES ('{}', '{team}', 'facebook', 'p{i}', '\\x00', {read})",
            Uuid::now_v7()
        ))
        .await
        .unwrap();
    }
    meta_inbox::tick(&state).await.unwrap();
    let r = db
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            format!(
                "SELECT count(*) FILTER (WHERE meta_synced_at > now() - interval '30 seconds') AS n \
                 FROM social_accounts WHERE team_id = '{team}'"
            ),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        r.try_get::<i64>("", "n").unwrap(),
        40,
        "the account read a minute ago is left"
    );
}
