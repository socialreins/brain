//! Boot and pool behaviour against a real Postgres at `DATABASE_URL` (see README): brain waits
//! for a database that is not there yet instead of exiting, and a full pool fails a request
//! after a few seconds instead of hanging.
use std::time::{Duration, Instant};

use sea_orm::{ConnectionTrait, Database, TransactionTrait};
use serial_test::serial;
use socialreins_brain::{config::Config, connect, connect_when_ready};

/// `DATABASE_URL` with its database name replaced.
fn url_for(config: &Config, name: &str) -> String {
    let (server, _) = config.database_url.rsplit_once('/').unwrap();
    format!("{server}/{name}")
}

#[tokio::test]
#[serial]
async fn boot_waits_for_a_database_that_comes_up_late() {
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    let name = format!("boot_{}", uuid::Uuid::now_v7().simple());
    let admin = Database::connect(&url_for(&config, "postgres"))
        .await
        .expect("a postgres database to create test databases in");
    config.database_url = url_for(&config, &name);
    // The database does not exist yet: one try fails at once, so a process that only tried
    // once would exit here.
    assert!(connect(&config).await.is_err());
    let later = {
        let admin = admin.clone();
        let name = name.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            admin
                .execute_unprepared(&format!("CREATE DATABASE {name}"))
                .await
                .unwrap();
        })
    };
    let started = Instant::now();
    let db = connect_when_ready(&config, Duration::from_secs(30), Duration::from_millis(200))
        .await
        .expect("connects once the database exists");
    later.await.unwrap();
    assert!(started.elapsed() >= Duration::from_secs(1));
    // It came up migrated.
    db.execute_unprepared("SELECT 1 FROM scheduler_ticks")
        .await
        .unwrap();
    db.close().await.unwrap();
    admin
        .execute_unprepared(&format!("DROP DATABASE {name}"))
        .await
        .unwrap();
}

#[tokio::test]
#[serial]
async fn boot_gives_up_after_the_wait() {
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    config.database_url = url_for(&config, "boot_never_created");
    let started = Instant::now();
    let res = connect_when_ready(&config, Duration::from_secs(2), Duration::from_millis(300)).await;
    assert!(res.is_err());
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_secs(6),
        "{took:?}"
    );
}

#[tokio::test]
#[serial]
async fn a_full_pool_fails_a_request_in_seconds_not_half_a_minute() {
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    config.database_max_connections = 2;
    let db = connect(&config).await.expect("database reachable");
    let (a, b) = (db.begin().await.unwrap(), db.begin().await.unwrap());
    let started = Instant::now();
    let res = db.execute_unprepared("SELECT 1").await;
    let took = started.elapsed();
    assert!(res.is_err(), "both connections are held");
    assert!(
        took >= Duration::from_secs(4) && took < Duration::from_secs(10),
        "{took:?}"
    );
    drop((a, b));
    db.execute_unprepared("SELECT 1")
        .await
        .expect("answers again once a connection is back");
}
