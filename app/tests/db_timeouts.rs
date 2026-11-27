//! The app pool's Postgres timeouts (`DATABASE_STATEMENT_TIMEOUT_SECONDS`,
//! `DATABASE_IDLE_IN_TRANSACTION_TIMEOUT_SECONDS`) and the migration pool's fixed lock timeout.
//! Needs `DATABASE_URL` like the request tests.
use std::time::Duration;

use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement, TransactionTrait};
use serial_test::serial;
use socialreins_brain::{config::Config, connect};

async fn show(db: &DatabaseConnection, setting: &str) -> String {
    let stmt = Statement::from_string(DbBackend::Postgres, format!("SHOW {setting}"));
    let row = db.query_one_raw(stmt).await.unwrap().unwrap();
    row.try_get_by_index(0).unwrap()
}

#[tokio::test]
#[serial]
async fn the_defaults_are_five_minutes_and_half_an_hour() {
    let config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    assert_eq!(config.database_statement_timeout, 300);
    assert_eq!(config.database_idle_in_transaction_timeout, 1800);
    let db = connect(&config).await.expect("database reachable");
    assert_eq!(show(&db, "statement_timeout").await, "5min");
    assert_eq!(
        show(&db, "idle_in_transaction_session_timeout").await,
        "30min"
    );
}

#[tokio::test]
#[serial]
async fn zero_turns_a_timeout_off() {
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    config.database_statement_timeout = 0;
    config.database_idle_in_transaction_timeout = 0;
    let db = connect(&config).await.expect("database reachable");
    assert_eq!(show(&db, "statement_timeout").await, "0");
    assert_eq!(show(&db, "idle_in_transaction_session_timeout").await, "0");
}

#[tokio::test]
#[serial]
async fn a_long_statement_and_a_forgotten_transaction_are_cut() {
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    config.database_statement_timeout = 1;
    config.database_idle_in_transaction_timeout = 1;
    let db = connect(&config).await.expect("database reachable");
    // Migrations already ran on their own connections: the pool is usable with the timeouts.
    let slow = db.execute_unprepared("SELECT pg_sleep(3)").await;
    let why = slow.expect_err("cut after a second").to_string();
    assert!(why.contains("statement timeout"), "{why}");
    // A transaction left idle (a lock held while a round works elsewhere) loses its session.
    let txn = db.begin().await.unwrap();
    txn.execute_unprepared("SELECT 1").await.unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(txn.execute_unprepared("SELECT 1").await.is_err());
    drop(txn);
    // The pool drops the dead connection and goes on.
    db.execute_unprepared("SELECT 1")
        .await
        .expect("answers on a new connection");
}

/// A migration that cannot take its table lock fails the boot instead of queueing every writer
/// behind it: the pool migrations run on carries the fixed 15 second timeout.
#[tokio::test]
#[serial]
async fn migrations_time_out_on_a_table_lock() {
    let config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    let db = socialreins_brain::migrating_pool(&config)
        .await
        .expect("database reachable");
    assert_eq!(show(&db, "lock_timeout").await, "15s");
    db.close().await.unwrap();
}
