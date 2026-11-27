//! The Postgres rate limiter (`rate_limit::RateLimiter`) against a real Postgres at
//! `DATABASE_URL` (see README).
use std::{
    net::IpAddr,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, Statement};
use serial_test::serial;
use socialreins_brain::{config::Config, connect, rate_limit::RateLimiter, state::AppState};
use uuid::Uuid;

async fn db() -> DatabaseConnection {
    let config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    connect(&config).await.expect("database reachable")
}

/// A prefix nothing else uses, so one test never shares a window with another.
fn prefix(tag: &str) -> String {
    format!("test-rate-limit:{tag}:{}", Uuid::now_v7().simple())
}

fn ip(s: &str) -> Option<IpAddr> {
    Some(s.parse().unwrap())
}

async fn clear(db: &DatabaseConnection, prefix: &str) {
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "DELETE FROM rate_limit_windows WHERE bucket LIKE $1",
        [format!("{prefix}:%").into()],
    );
    if let Err(e) = db.execute_raw(stmt).await {
        panic!("cleanup failed: {e}");
    }
}

async fn window_start(db: &DatabaseConnection, bucket: &str) -> DateTime<Utc> {
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT window_start FROM rate_limit_windows WHERE bucket = $1",
        [bucket.to_owned().into()],
    );
    let row = db
        .query_one_raw(stmt)
        .await
        .expect("window readable")
        .expect("window found");
    row.try_get("", "window_start").expect("window_start")
}

/// Two replicas, each with its own connection pool and its own limiter, count one window.
#[tokio::test]
#[serial]
async fn two_instances_share_one_window() {
    let (a, b) = (db().await, db().await);
    let name = prefix("shared");
    let one = RateLimiter::per_minute(name.clone(), 2);
    let two = RateLimiter::per_minute(name.clone(), 2);
    assert!(one.allow(&a, ip("203.0.113.42")).await);
    assert!(
        two.allow(&b, ip("203.0.113.42")).await,
        "the second instance sees the first instance's hit"
    );
    assert!(!one.allow(&a, ip("203.0.113.42")).await);
    assert!(!two.allow(&b, ip("203.0.113.42")).await);
    assert!(
        two.allow(&b, ip("203.0.113.43")).await,
        "another network is another bucket"
    );
    clear(&a, &name).await;
}

#[tokio::test]
#[serial]
async fn a_second_check_in_the_window_is_refused() {
    let db = db().await;
    let name = prefix("refused");
    let l = RateLimiter::per_minute(name.clone(), 1);
    assert!(l.allow(&db, ip("203.0.113.50")).await);
    assert!(!l.allow(&db, ip("203.0.113.50")).await);
    let left = l
        .wait(&db, "203.0.113.50".parse::<IpAddr>().unwrap())
        .await
        .expect("over the cap");
    assert!(
        left > Duration::from_secs(55) && left <= Duration::from_secs(60),
        "{left:?}"
    );
    clear(&db, &name).await;
}

#[tokio::test]
#[serial]
async fn the_window_rolls() {
    let db = db().await;
    let name = prefix("rolls");
    let l = RateLimiter::new(name.clone(), 1, Duration::from_secs(1));
    assert!(l.allow(&db, ip("203.0.113.51")).await);
    assert!(!l.allow(&db, ip("203.0.113.51")).await);
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert!(
        l.allow(&db, ip("203.0.113.51")).await,
        "a new window allows again"
    );
    clear(&db, &name).await;
}

/// A refused check counts, but it never moves the window, so it cannot hold a client out longer
/// than one window from its first hit.
#[tokio::test]
#[serial]
async fn a_refused_check_does_not_extend_the_window() {
    let db = db().await;
    let name = prefix("not-extended");
    let bucket = format!("{name}:203.0.113.52");
    let l = RateLimiter::new(name.clone(), 1, Duration::from_secs(2));
    assert!(l.allow(&db, ip("203.0.113.52")).await);
    let first = window_start(&db, &bucket).await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert!(!l.allow(&db, ip("203.0.113.52")).await);
    assert_eq!(
        window_start(&db, &bucket).await,
        first,
        "the refused check left the window where it was"
    );
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        l.allow(&db, ip("203.0.113.52")).await,
        "the window rolled from the first hit, not from the refused one"
    );
    clear(&db, &name).await;
}

#[tokio::test]
#[serial]
async fn a_database_error_lets_the_request_through() {
    let db = db().await;
    let name = prefix("fail-open");
    let l = RateLimiter::per_minute(name.clone(), 1);
    assert!(l.allow(&db, ip("203.0.113.60")).await);
    db.clone().close().await.unwrap();
    assert!(
        l.allow(&db, ip("203.0.113.60")).await,
        "a limiter that cannot reach Postgres fails open"
    );
    assert_eq!(
        l.wait(&db, "203.0.113.60".parse::<IpAddr>().unwrap()).await,
        None
    );
}

/// Billing off (the default in tests) leaves every limit as it was: the limiter is not part of
/// the billing gate, and the invitation burst is still 10 a minute per user across instances.
#[tokio::test]
#[serial]
async fn billing_off_keeps_the_same_limits() {
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    config.billing_enabled = false;
    assert!(!config.billing_enabled);
    let db = connect(&config).await.expect("database reachable");
    let one = AppState::new(db.clone(), &config);
    let two = AppState::new(db.clone(), &config);
    let user = Uuid::now_v7();
    for _ in 0..10 {
        assert!(one.invitation_limiter.hit(&db, user).await);
    }
    assert!(
        !two.invitation_limiter.hit(&db, user).await,
        "the 10 a minute invitation limit is unchanged and shared"
    );
    let stmt = Statement::from_sql_and_values(
        DbBackend::Postgres,
        "DELETE FROM rate_limit_windows WHERE bucket = $1",
        [format!("invitation:{user}").into()],
    );
    db.execute_raw(stmt).await.expect("cleanup");
}

/// 1,000 buckets already tracked do not slow a check: it is one index lookup and one row
/// update, not a scan of the table. Interleaved with the same loop on a single bucket, a check
/// took about 0.30 ms either way (local Postgres 17, debug build, 200 checks each).
#[tokio::test]
#[serial]
async fn a_thousand_buckets_do_not_slow_a_check() {
    let db = db().await;
    let name = prefix("many");
    let l = RateLimiter::per_minute(name.clone(), 10_000);
    for i in 0..1_000u128 {
        assert!(l.hit(&db, Uuid::from_u128(i + 1)).await);
    }
    // Warm the hourly prune and the pool's prepared statement before timing.
    assert!(l.hit(&db, Uuid::nil()).await);
    let base_name = prefix("many-baseline");
    let base = RateLimiter::per_minute(base_name.clone(), 10_000);
    assert!(base.hit(&db, Uuid::nil()).await);
    // Interleaved, so a busy moment in the database falls on both loops.
    let checks = 200u32;
    let (mut many, mut one) = (Duration::ZERO, Duration::ZERO);
    for i in 0..checks {
        let key = Uuid::from_u128(u128::from(i % 1_000) + 1);
        let started = Instant::now();
        assert!(l.hit(&db, key).await);
        many += started.elapsed();
        let started = Instant::now();
        assert!(base.hit(&db, Uuid::nil()).await);
        one += started.elapsed();
    }
    let (each, one) = (many / checks, one / checks);
    // A check is one index lookup and one row update, not a scan of the 1,000 rows.
    assert!(
        each < one * 3 + Duration::from_millis(2),
        "1,000 buckets took {each:?} each, one bucket took {one:?}"
    );
    clear(&db, &name).await;
    clear(&db, &base_name).await;
}
