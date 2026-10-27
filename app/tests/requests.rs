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
async fn bad_bodies_get_json_errors() {
    let (s, _) = server().await;
    let json_body = |raw: &'static str| {
        s.post("/v1/waitlist")
            .content_type("application/json")
            .bytes(raw.into())
    };
    let cases = [
        (json_body("{not json").await, 400, "invalid_json"),
        (json_body(r#"{"source":"x"}"#).await, 422, "invalid_body"),
        (json_body(r#"{"email":42}"#).await, 422, "invalid_body"),
        (
            s.post("/v1/waitlist").text("a@example.com").await,
            415,
            "unsupported_media_type",
        ),
    ];
    for (res, status, code) in cases {
        assert_eq!(res.status_code(), status, "{code}");
        res.assert_json(&json!({"error": code}));
    }
}

#[tokio::test]
#[serial]
async fn bodies_over_4_kib_are_refused() {
    let (s, _) = server().await;
    let email = format!("{}@example.com", "a".repeat(4100));
    let res = s.post("/v1/waitlist").json(&json!({"email": email})).await;
    assert_eq!(res.status_code(), 413);
    res.assert_json(&json!({"error": "payload_too_large"}));
}

#[tokio::test]
#[serial]
async fn trusted_proxy_without_forwarded_for_falls_back_to_the_peer() {
    // No header and no socket peer under the in-memory transport: one bucket, still limited.
    let (s, _) = server_with(true).await;
    for _ in 0..10 {
        let req = s
            .post("/v1/waitlist")
            .json(&json!({"email": unique_email()}));
        req.await.assert_status_ok();
    }
    let res = s
        .post("/v1/waitlist")
        .add_header("x-forwarded-for", "garbage");
    assert_eq!(
        res.json(&json!({"email": unique_email()}))
            .await
            .status_code(),
        429
    );
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

mod auth {
    use super::*;
    use chrono::{Duration, Utc};
    use sea_orm::{ActiveValue::Set, DatabaseConnection};
    use socialreins_brain::models::{magic_links, users};
    use socialreins_core::token;

    async fn server_where(edit: impl FnOnce(&mut Config)) -> (TestServer, DatabaseConnection) {
        let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
        edit(&mut config);
        let db = connect(&config).await.expect("database reachable");
        let app = controllers::router(AppState::new(db.clone(), &config), &config);
        (TestServer::new(app).unwrap(), db)
    }

    /// Stores a link for `email` the way `POST /auth/link` does and returns its token.
    async fn link(db: &DatabaseConnection, email: &str, valid_for: Duration) -> String {
        let secret = token::new();
        let row = magic_links::ActiveModel {
            token_hash: Set(token::hash(&secret)),
            email: Set(email.into()),
            expires_at: Set((Utc::now() + valid_for).fixed_offset()),
            ..Default::default()
        };
        magic_links::Entity::insert(row)
            .exec_without_returning(db)
            .await
            .unwrap();
        secret
    }

    async fn links(db: &DatabaseConnection, email: &str) -> u64 {
        magic_links::Entity::find()
            .filter(magic_links::Column::Email.eq(email))
            .count(db)
            .await
            .unwrap()
    }

    async fn user(db: &DatabaseConnection, email: &str) -> Option<users::Model> {
        users::Entity::find()
            .filter(users::Column::Email.eq(email))
            .one(db)
            .await
            .unwrap()
    }

    /// Redeems `secret`, returning the redirect target and the session cookie it set, if any.
    async fn open(s: &TestServer, secret: &str) -> (String, Option<String>) {
        let res = s.get(&format!("/auth/magic/{secret}")).await;
        assert_eq!(res.status_code(), 303);
        let cookie = res
            .maybe_header("set-cookie")
            .map(|c| c.to_str().unwrap().to_owned());
        (res.header("location").to_str().unwrap().to_owned(), cookie)
    }

    fn session(set_cookie: &str) -> String {
        set_cookie.split(';').next().unwrap().to_owned()
    }

    #[tokio::test]
    #[serial]
    async fn link_request_answers_the_same_for_new_and_known_emails() {
        let (s, db) = server_where(|_| {}).await;
        let email = unique_email();
        let first = s.post("/auth/link").json(&json!({"email": email})).await;
        let again = s
            .post("/auth/link")
            .json(&json!({"email": email.to_uppercase()}))
            .await;
        assert_eq!(first.status_code(), 202);
        assert_eq!(first.json::<Value>(), json!({"status": "sent"}));
        assert_eq!(first.json::<Value>(), again.json::<Value>());
        assert_eq!(links(&db, &email).await, 2);
        assert!(
            user(&db, &email).await.is_none(),
            "account waits for the link"
        );
        let bad = s.post("/auth/link").json(&json!({"email": "nope"})).await;
        assert_eq!(bad.status_code(), 422);
    }

    #[tokio::test]
    #[serial]
    async fn link_signs_up_once_and_me_returns_the_user() {
        let (s, db) = server_where(|_| {}).await;
        let email = unique_email();
        let secret = link(&db, &email, Duration::minutes(15)).await;
        let (to, cookie) = open(&s, &secret).await;
        assert_eq!(to, "http://localhost:3000");
        let cookie = cookie.expect("session cookie");
        assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"));
        assert!(!cookie.contains("Secure"), "http WEB_URL drops Secure");
        let me = s
            .get("/auth/me")
            .add_header("cookie", session(&cookie))
            .await;
        me.assert_status_ok();
        assert_eq!(me.json::<Value>()["email"], email);

        let (to, cookie) = open(&s, &secret).await;
        assert_eq!(to, "http://localhost:3000/login?error=invalid_link");
        assert!(cookie.is_none());
        assert_eq!(links(&db, &email).await, 0);
    }

    #[tokio::test]
    #[serial]
    async fn expired_link_is_refused() {
        let (s, db) = server_where(|_| {}).await;
        let email = unique_email();
        let secret = link(&db, &email, Duration::minutes(-1)).await;
        let (to, cookie) = open(&s, &secret).await;
        assert!(to.ends_with("/login?error=invalid_link"));
        assert!(cookie.is_none());
        assert!(user(&db, &email).await.is_none());
    }

    #[tokio::test]
    #[serial]
    async fn registration_disabled_still_lets_existing_users_in() {
        let (open_server, db) = server_where(|_| {}).await;
        let known = unique_email();
        let (_, cookie) = open(
            &open_server,
            &link(&db, &known, Duration::minutes(15)).await,
        )
        .await;
        assert!(cookie.is_some());

        let (s, _) = server_where(|c| c.registration_disabled = true).await;
        let stranger = unique_email();
        let res = s.post("/auth/link").json(&json!({"email": stranger})).await;
        assert_eq!(res.status_code(), 202);
        assert_eq!(res.json::<Value>(), json!({"status": "sent"}));
        assert_eq!(links(&db, &stranger).await, 0, "no link for a new email");
        // A link made before sign-up closed no longer creates the account.
        let early = link(&db, &stranger, Duration::minutes(15)).await;
        let (to, cookie) = open(&s, &early).await;
        assert!(to.ends_with("/login?error=registration_disabled"));
        assert!(cookie.is_none() && user(&db, &stranger).await.is_none());

        s.post("/auth/link")
            .json(&json!({"email": known}))
            .await
            .assert_status(axum::http::StatusCode::ACCEPTED);
        assert_eq!(links(&db, &known).await, 1);
        let (_, cookie) = open(&s, &link(&db, &known, Duration::minutes(15)).await).await;
        assert!(cookie.is_some());
    }

    #[tokio::test]
    #[serial]
    async fn logout_invalidates_the_session() {
        let (s, db) = server_where(|c| c.web_url = "https://app.example".into()).await;
        let (to, cookie) = open(&s, &link(&db, &unique_email(), Duration::minutes(15)).await).await;
        assert_eq!(to, "https://app.example");
        let cookie = cookie.unwrap();
        assert!(cookie.contains("; Secure"));
        let me = || s.get("/auth/me").add_header("cookie", session(&cookie));
        me().await.assert_status_ok();
        let out = s
            .post("/auth/logout")
            .add_header("cookie", session(&cookie))
            .await;
        assert_eq!(out.status_code(), 204);
        assert!(out
            .header("set-cookie")
            .to_str()
            .unwrap()
            .contains("Max-Age=0"));
        assert_eq!(me().await.status_code(), 401);
    }

    #[tokio::test]
    #[serial]
    async fn me_without_a_session_is_401() {
        let (s, _) = server_where(|_| {}).await;
        let res = s.get("/auth/me").await;
        assert_eq!(res.status_code(), 401);
        res.assert_json(&json!({"error": "unauthorized"}));
        let forged = s
            .get("/auth/me")
            .add_header("cookie", "sr_session=forged")
            .await;
        assert_eq!(forged.status_code(), 401);
    }

    #[tokio::test]
    #[serial]
    async fn link_requests_are_rate_limited() {
        let (s, _) = server_where(|_| {}).await;
        for _ in 0..5 {
            let res = s
                .post("/auth/link")
                .json(&json!({"email": unique_email()}))
                .await;
            assert_eq!(res.status_code(), 202);
        }
        let res = s
            .post("/auth/link")
            .json(&json!({"email": unique_email()}))
            .await;
        assert_eq!(res.status_code(), 429);
        assert_eq!(s.get("/auth/magic/whatever").await.status_code(), 429);
    }
}
