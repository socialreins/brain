//! Request tests against a real Postgres at `DATABASE_URL` (see README).
use axum_test::TestServer;
use sea_orm::{ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter};
use serde_json::{json, Value};
use serial_test::serial;
use socialreins_brain::{
    config::Config, connect, controllers, models::waitlist_signups, state::AppState,
};

/// A fresh app (fresh rate limiter) over the migrated test database.
async fn server() -> (TestServer, sea_orm::DatabaseConnection) {
    server_with(false).await
}

async fn server_with(trust_proxy: bool) -> (TestServer, sea_orm::DatabaseConnection) {
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    config.git_commit = "dev".into();
    config.trust_proxy = trust_proxy;
    let db = connect(&config).await.expect("database reachable");
    let app = controllers::router(AppState::new(db.clone(), &config), &config);
    (TestServer::new(app).unwrap(), db)
}

fn unique_email() -> String {
    format!("{}@example.com", uuid::Uuid::now_v7())
}

async fn rows(db: &sea_orm::DatabaseConnection, email: &str) -> u64 {
    waitlist_signups::Entity::find()
        .filter(waitlist_signups::Column::Email.eq(email))
        .count(db)
        .await
        .unwrap()
}

#[tokio::test]
#[serial]
async fn health_and_version() {
    let (s, _) = server().await;
    let res = s.get("/health").await;
    res.assert_status_ok();
    res.assert_json(&json!({"status": "ok"}));
    s.get("/version")
        .await
        .assert_json(&json!({"commit": "dev"}));
}

#[tokio::test]
#[serial]
async fn waitlist_signup_is_stored_lowercased() {
    let (s, db) = server().await;
    let email = unique_email();
    let res = s
        .post("/v1/waitlist")
        .json(&json!({"email": format!("  {}  ", email.to_uppercase()), "source": "landing"}))
        .await;
    res.assert_status_ok();
    res.assert_json(&json!({"status": "ok"}));
    let row = waitlist_signups::Entity::find()
        .filter(waitlist_signups::Column::Email.eq(&email))
        .one(&db)
        .await
        .unwrap()
        .expect("row stored");
    assert_eq!(row.source.as_deref(), Some("landing"));
}

#[tokio::test]
#[serial]
async fn duplicate_signup_answers_the_same_and_keeps_one_row() {
    let (s, db) = server().await;
    let email = unique_email();
    let first = s.post("/v1/waitlist").json(&json!({"email": email})).await;
    let again = s
        .post("/v1/waitlist")
        .json(&json!({"email": email.to_uppercase(), "source": "footer"}))
        .await;
    assert_eq!(first.status_code(), 200);
    assert_eq!(again.status_code(), 200);
    assert_eq!(first.json::<Value>(), again.json::<Value>());
    assert_eq!(rows(&db, &email).await, 1);
}

#[tokio::test]
#[serial]
async fn invalid_email_is_rejected() {
    let (s, _) = server().await;
    for bad in ["", "not-an-email", "a@b", "a b@c.com"] {
        let res = s.post("/v1/waitlist").json(&json!({"email": bad})).await;
        assert_eq!(res.status_code(), 422, "{bad}");
        res.assert_json(&json!({"error": "invalid_email"}));
    }
}

#[tokio::test]
#[serial]
async fn signups_are_rate_limited_per_ip() {
    let (s, _) = server_with(true).await;
    let post = |ip: &'static str| {
        s.post("/v1/waitlist")
            .add_header("x-forwarded-for", ip)
            .json(&json!({"email": unique_email()}))
    };
    for _ in 0..10 {
        post("203.0.113.9").await.assert_status_ok();
    }
    assert_eq!(post("203.0.113.9").await.status_code(), 429);
    post("203.0.113.10").await.assert_status_ok();
}

#[tokio::test]
#[serial]
async fn spoofed_forwarded_for_is_ignored_without_trust_proxy() {
    let (s, _) = server().await;
    for i in 0..10 {
        s.post("/v1/waitlist")
            .add_header("x-forwarded-for", format!("198.51.100.{i}"))
            .json(&json!({"email": unique_email()}))
            .await
            .assert_status_ok();
    }
    let res = s
        .post("/v1/waitlist")
        .add_header("x-forwarded-for", "198.51.100.200")
        .json(&json!({"email": unique_email()}))
        .await;
    assert_eq!(res.status_code(), 429);
}

#[tokio::test]
#[serial]
async fn cors_allows_the_site_only() {
    let (s, _) = server().await;
    let preflight = |origin: &'static str| {
        s.method(axum::http::Method::OPTIONS, "/v1/waitlist")
            .add_header("origin", origin)
            .add_header("access-control-request-method", "POST")
    };
    let ok = preflight("https://socialreins.com").await;
    assert_eq!(
        ok.header("access-control-allow-origin"),
        "https://socialreins.com"
    );
    let other = preflight("https://evil.example").await;
    assert!(other.maybe_header("access-control-allow-origin").is_none());
}

#[tokio::test]
#[serial]
async fn openapi_documents_the_waitlist() {
    let (s, _) = server().await;
    let doc = s.get("/openapi.json").await.json::<Value>();
    assert!(doc["paths"]["/waitlist"]["post"].is_object(), "{doc}");
}
