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
async fn favicon_and_icon_are_served() {
    let (s, _) = server().await;
    let ico = s.get("/favicon.ico").await;
    ico.assert_status_ok();
    assert_eq!(ico.header("content-type"), "image/x-icon");
    let svg = s.get("/icon.svg").await;
    svg.assert_status_ok();
    assert_eq!(svg.header("content-type"), "image/svg+xml");
    assert!(svg.text().contains("#ff5b22"));
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
    let ok = preflight("http://localhost:3000").await;
    assert_eq!(
        ok.header("access-control-allow-origin"),
        "http://localhost:3000"
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
    use socialreins_brain::models::{magic_links, sessions, users};
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

    /// Redeems `secret` the way the button on the link page does, returning the redirect target
    /// and the session cookie it set, if any.
    async fn open(s: &TestServer, secret: &str) -> (String, Option<String>) {
        open_from(s, secret, "http://localhost:8080").await
    }

    /// `open`, posting from the page at `origin` (brain's own, unless BRAIN_URL was changed).
    async fn open_from(s: &TestServer, secret: &str, origin: &str) -> (String, Option<String>) {
        let res = s
            .post("/auth/magic")
            .add_header("origin", origin)
            .form(&[("token", secret)])
            .await;
        assert_eq!(res.status_code(), 303);
        let cookie = res
            .maybe_header("set-cookie")
            .map(|c| c.to_str().unwrap().to_owned());
        (res.header("location").to_str().unwrap().to_owned(), cookie)
    }

    /// Waits for the background task of `POST /auth/link` to store `want` links for `email`.
    async fn links_eventually(db: &DatabaseConnection, email: &str, want: u64) -> u64 {
        for _ in 0..50 {
            if links(db, email).await >= want {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        links(db, email).await
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
        assert_eq!(links_eventually(&db, &email, 2).await, 2);
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
        assert!(!cookie.contains("Secure"), "http BRAIN_URL drops Secure");
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
    async fn opening_the_link_only_shows_a_button() {
        let (s, db) = server_where(|_| {}).await;
        let email = unique_email();
        let secret = link(&db, &email, Duration::minutes(15)).await;
        for _ in 0..2 {
            let page = s.get(&format!("/auth/magic/{secret}")).await;
            page.assert_status_ok();
            assert!(page.maybe_header("set-cookie").is_none());
            let html = page.text();
            assert!(html.contains(r#"action="/auth/magic""#) && html.contains(&secret));
        }
        assert_eq!(links(&db, &email).await, 1, "GET leaves the link usable");
        assert!(user(&db, &email).await.is_none());
        let (_, cookie) = open(&s, &secret).await;
        assert!(cookie.is_some());
        let junk = s.get("/auth/magic/%3Cscript%3E").await;
        assert_eq!(junk.status_code(), 303);
    }

    #[tokio::test]
    #[serial]
    async fn redeem_from_another_origin_is_refused() {
        let (s, db) = server_where(|_| {}).await;
        let email = unique_email();
        let secret = link(&db, &email, Duration::minutes(15)).await;
        let post = || s.post("/auth/magic").form(&[("token", secret.as_str())]);
        let foreign = post().add_header("origin", "https://evil.example").await;
        assert_eq!(foreign.status_code(), 403);
        foreign.assert_json(&json!({"error": "forbidden_origin"}));
        assert_eq!(post().await.status_code(), 403, "no Origin or Referer");
        let referer = post()
            .add_header("referer", "https://evil.example/auth/magic/x")
            .await;
        assert_eq!(referer.status_code(), 403);
        assert_eq!(
            links(&db, &email).await,
            1,
            "a refused post leaves the link"
        );
        let ok = post()
            .add_header(
                "referer",
                format!("http://localhost:8080/auth/magic/{secret}"),
            )
            .await;
        assert_eq!(ok.status_code(), 303);
        assert!(ok.maybe_header("set-cookie").is_some());
    }

    #[tokio::test]
    #[serial]
    async fn one_email_holds_at_most_three_live_links() {
        let (s, db) = server_where(|_| {}).await;
        let (capped, other) = (unique_email(), unique_email());
        for _ in 0..3 {
            link(&db, &capped, Duration::minutes(15)).await;
        }
        link(&db, &capped, Duration::minutes(-1)).await;
        let ask = |email: &str| s.post("/auth/link").json(&json!({"email": email}));
        assert_eq!(ask(&capped).await.status_code(), 202);
        assert_eq!(ask(&other).await.status_code(), 202);
        assert_eq!(links_eventually(&db, &other, 1).await, 1);
        // The capped request came first, so its background task has finished by now; the
        // expired link was swept by one of the two tasks.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(links(&db, &capped).await, 3);
    }

    #[tokio::test]
    #[serial]
    async fn concurrent_link_requests_respect_the_cap() {
        let (s, db) = server_where(|_| {}).await;
        let email = unique_email();
        // Each reply returns before its background insert, so the five inserts race.
        for _ in 0..5 {
            let res = s.post("/auth/link").json(&json!({"email": email})).await;
            assert_eq!(res.status_code(), 202);
        }
        assert_eq!(links_eventually(&db, &email, 3).await, 3);
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(links(&db, &email).await, 3);
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
        // A link made before sign-up closed no longer creates the account.
        let early = link(&db, &stranger, Duration::minutes(15)).await;
        let (to, cookie) = open(&s, &early).await;
        assert!(to.ends_with("/login?error=registration_disabled"));
        assert!(cookie.is_none() && user(&db, &stranger).await.is_none());

        s.post("/auth/link")
            .json(&json!({"email": known}))
            .await
            .assert_status(axum::http::StatusCode::ACCEPTED);
        assert_eq!(links_eventually(&db, &known, 1).await, 1);
        // The stranger asked first, so its background task has finished by now.
        assert_eq!(links(&db, &stranger).await, 0, "no link for a new email");
        let (_, cookie) = open(&s, &link(&db, &known, Duration::minutes(15)).await).await;
        assert!(cookie.is_some());
    }

    #[tokio::test]
    #[serial]
    async fn logout_invalidates_the_session() {
        let (s, db) = server_where(|c| {
            c.web_url = "https://app.example".into();
            c.brain_url = "https://api.example".into();
        })
        .await;
        let secret = link(&db, &unique_email(), Duration::minutes(15)).await;
        let (to, cookie) = open_from(&s, &secret, "https://api.example").await;
        assert_eq!(to, "https://app.example");
        let cookie = cookie.unwrap();
        assert!(cookie.contains("; Secure"));
        let me = || s.get("/auth/me").add_header("cookie", session(&cookie));
        me().await.assert_status_ok();
        let logout = || {
            s.post("/auth/logout")
                .add_header("cookie", session(&cookie))
        };
        for foreign in [None, Some("https://evil.example")] {
            let res = match foreign {
                Some(o) => logout().add_header("origin", o).await,
                None => logout().await,
            };
            assert_eq!(res.status_code(), 403);
            res.assert_json(&json!({"error": "forbidden_origin"}));
        }
        me().await.assert_status_ok();
        // CORS_ORIGINS keeps its env default here, not the edited WEB_URL.
        let out = logout().add_header("origin", "http://localhost:3000").await;
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
        let redeem = s.post("/auth/magic").form(&[("token", "x")]).await;
        assert_eq!(redeem.status_code(), 429);
    }

    const WEB: &str = "http://localhost:3000";

    /// Signs a new user in by link, returning their email and the `sr_session=...` cookie.
    async fn signed_in(s: &TestServer, db: &DatabaseConnection) -> (String, String) {
        let email = unique_email();
        let (_, cookie) = open(s, &link(db, &email, Duration::minutes(15)).await).await;
        (email, session(&cookie.unwrap()))
    }

    /// Backdates the session behind `cookie`, so it no longer counts as a recent sign-in.
    async fn age(db: &DatabaseConnection, cookie: &str, by: Duration) {
        let hash = token::hash(cookie.trim_start_matches("sr_session="));
        let row = sessions::ActiveModel {
            token_hash: Set(hash),
            created_at: Set((Utc::now() - by).fixed_offset()),
            ..Default::default()
        };
        sessions::Entity::update(row).exec(db).await.unwrap();
    }

    #[tokio::test]
    #[serial]
    async fn passkey_enrolment_needs_a_recent_sign_in() {
        let (s, db) = server_where(|_| {}).await;
        let (_, cookie) = signed_in(&s, &db).await;
        age(&db, &cookie, Duration::minutes(11)).await;
        let start = s
            .post("/auth/passkeys/register/start")
            .add_header("origin", WEB)
            .add_header("cookie", &cookie)
            .json(&json!({}))
            .await;
        assert_eq!(start.status_code(), 403);
        start.assert_json(&json!({"error": "reauth_required"}));
        // A fresh sign-in is enough; there are no passwords.
        age(&db, &cookie, Duration::zero()).await;
        let start = s
            .post("/auth/passkeys/register/start")
            .add_header("origin", WEB)
            .add_header("cookie", &cookie)
            .await;
        start.assert_status_ok();
        for gone in ["/auth/login", "/auth/password"] {
            let res = s
                .post(gone)
                .add_header("origin", WEB)
                .json(&json!({}))
                .await;
            assert_eq!(res.status_code(), 404, "{gone}");
        }
        let me = s.get("/auth/me").add_header("cookie", &cookie).await;
        assert!(me.json::<Value>().get("has_password").is_none());
    }

    #[tokio::test]
    #[serial]
    async fn credential_endpoints_only_answer_the_web_origin() {
        let (s, db) = server_where(|_| {}).await;
        let (_, cookie) = signed_in(&s, &db).await;
        for origin in [
            None,
            Some("https://evil.example"),
            Some("http://localhost:8080"),
        ] {
            let calls = [
                s.post("/auth/passkeys/login/start"),
                s.post("/auth/passkeys/register/start")
                    .add_header("cookie", &cookie)
                    .json(&json!({})),
                s.delete(&format!("/auth/passkeys/{}", uuid::Uuid::now_v7()))
                    .add_header("cookie", &cookie),
            ];
            for call in calls {
                let res = match origin {
                    Some(o) => call.add_header("origin", o).await,
                    None => call.await,
                };
                assert_eq!(res.status_code(), 403, "{origin:?}");
                res.assert_json(&json!({"error": "forbidden_origin"}));
            }
        }
        let preflight = s
            .method(axum::http::Method::OPTIONS, "/v1/team")
            .add_header("origin", WEB)
            .add_header("access-control-request-method", "PUT")
            .await;
        assert!(preflight
            .header("access-control-allow-methods")
            .to_str()
            .unwrap()
            .contains("PUT"));
    }

    /// A server over a throwaway database with no users yet (the shared test database already
    /// has some). Returns the database name for `drop_db`.
    async fn empty_db_server(
        edit: impl FnOnce(&mut Config),
    ) -> (TestServer, DatabaseConnection, String) {
        use sea_orm::ConnectionTrait;
        let base = Config::from_env().unwrap().database_url;
        let name = format!("brain_first_{}", uuid::Uuid::now_v7().simple());
        let admin = sea_orm::Database::connect(&base).await.unwrap();
        admin
            .execute_unprepared(&format!("CREATE DATABASE {name}"))
            .await
            .unwrap();
        let url = format!("{}/{name}", &base[..base.rfind('/').unwrap()]);
        let (s, db) = server_where(|c| {
            c.database_url = url;
            c.registration_disabled = true;
            edit(c);
        })
        .await;
        (s, db, name)
    }

    async fn drop_db(db: DatabaseConnection, name: &str) {
        use sea_orm::ConnectionTrait;
        db.close().await.unwrap();
        let admin = sea_orm::Database::connect(Config::from_env().unwrap().database_url)
            .await
            .unwrap();
        admin
            .execute_unprepared(&format!("DROP DATABASE {name}"))
            .await
            .unwrap();
    }

    async fn users_count(db: &DatabaseConnection) -> u64 {
        users::Entity::find().count(db).await.unwrap()
    }

    /// Without OWNER_EMAIL, a closed install lets its first account in, then no stranger.
    #[tokio::test]
    #[serial]
    async fn closed_registration_admits_only_the_first_account() {
        let (s, db, name) = empty_db_server(|_| {}).await;
        let first = unique_email();
        let (_, cookie) = open(&s, &link(&db, &first, Duration::minutes(15)).await).await;
        assert!(cookie.is_some(), "the first account signs up");
        let second = unique_email();
        let (to, cookie) = open(&s, &link(&db, &second, Duration::minutes(15)).await).await;
        assert!(to.ends_with("/login?error=registration_disabled"));
        assert!(cookie.is_none() && user(&db, &second).await.is_none());
        drop_db(db, &name).await;
    }

    /// Strangers racing for the first account take turns on an advisory lock: while another
    /// sign-up holds it, a redemption waits, then sees that account and is refused.
    #[tokio::test]
    #[serial]
    async fn first_sign_up_waits_for_the_lock() {
        use sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};
        let (s, db, name) = empty_db_server(|_| {}).await;
        let secret = link(&db, &unique_email(), Duration::minutes(15)).await;
        let holder = db.begin().await.unwrap();
        holder
            .execute_raw(Statement::from_string(
                DbBackend::Postgres,
                "SELECT pg_advisory_xact_lock(x'0053524649525354'::bigint)",
            ))
            .await
            .unwrap();
        let mut redeem = std::pin::pin!(open(&s, &secret));
        let waited = tokio::time::timeout(std::time::Duration::from_millis(300), &mut redeem).await;
        assert!(waited.is_err(), "the redemption waits for the lock");
        users::Entity::insert(users::ActiveModel {
            id: Set(uuid::Uuid::now_v7()),
            email: Set(unique_email()),
            name: Set("first".into()),
            ..Default::default()
        })
        .exec_without_returning(&holder)
        .await
        .unwrap();
        holder.commit().await.unwrap();
        let (to, cookie) = redeem.await;
        assert!(to.ends_with("/login?error=registration_disabled"));
        assert!(cookie.is_none());
        assert_eq!(users_count(&db).await, 1);
        drop_db(db, &name).await;
    }

    /// With OWNER_EMAIL, only the owner (and invited emails) sign up, even on an empty install.
    #[tokio::test]
    #[serial]
    async fn owner_email_is_the_only_stranger_admitted() {
        let owner = unique_email();
        let (s, db, name) = empty_db_server(|c| c.owner_email = Some(owner.clone())).await;
        let stranger = unique_email();
        let (to, cookie) = open(&s, &link(&db, &stranger, Duration::minutes(15)).await).await;
        assert!(to.ends_with("/login?error=registration_disabled"));
        assert!(cookie.is_none() && users_count(&db).await == 0);
        s.post("/auth/link")
            .json(&json!({"email": stranger}))
            .await
            .assert_status(axum::http::StatusCode::ACCEPTED);
        s.post("/auth/link")
            .json(&json!({"email": owner.to_uppercase()}))
            .await
            .assert_status(axum::http::StatusCode::ACCEPTED);
        assert_eq!(links_eventually(&db, &owner, 1).await, 1);
        assert_eq!(links(&db, &stranger).await, 0, "no link for the stranger");
        let (_, cookie) = open(&s, &link(&db, &owner, Duration::minutes(15)).await).await;
        assert!(cookie.is_some(), "the owner signs up");
        drop_db(db, &name).await;
    }

    /// Google sign-in against a mock token endpoint. The test passes the id_token claims as the
    /// `code`; the mock adds issuer, audience and expiry unless the claims set them.
    mod google {
        use super::*;
        use axum::{routing::post, Form, Json, Router};
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        use socialreins_brain::connectors::google::Google;
        use std::collections::HashMap;

        static VERIFIERS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

        async fn mock_token() -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let token = |Form(f): Form<HashMap<String, String>>| async move {
                let ok = f.get("client_secret").map(String::as_str) == Some("secret")
                    && f.get("grant_type").map(String::as_str) == Some("authorization_code");
                let code = f.get("code").cloned().unwrap_or_default();
                let claims = URL_SAFE_NO_PAD
                    .decode(&code)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
                let (true, Some(Value::Object(given))) = (ok, claims) else {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(json!({"error": "invalid_grant"})),
                    );
                };
                VERIFIERS
                    .lock()
                    .unwrap()
                    .push(f.get("code_verifier").cloned().unwrap_or_default());
                let mut c = json!({"iss": "https://accounts.google.com", "aud": "cid",
                    "exp": Utc::now().timestamp() + 3600});
                for (k, v) in given {
                    c[k] = v;
                }
                let jwt = format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(c.to_string()));
                (
                    axum::http::StatusCode::OK,
                    Json(json!({"id_token": jwt, "access_token": "at"})),
                )
            };
            let app = Router::new().route("/token", post(token));
            tokio::spawn(async move { axum::serve(listener, app).await });
            base
        }

        async fn google_server(edit: impl FnOnce(&mut Config)) -> (TestServer, DatabaseConnection) {
            let base = mock_token().await;
            server_where(|c| {
                let mut g = Google::new("cid", "secret").unwrap();
                g.token_url = format!("{base}/token");
                c.google = Some(g);
                edit(c);
            })
            .await
        }

        /// The cookie `name=value` among the response's `Set-Cookie` headers.
        fn set_cookie(res: &axum_test::TestResponse, name: &str) -> Option<String> {
            res.iter_headers_by_name("set-cookie")
                .map(|v| v.to_str().unwrap().to_owned())
                .find(|c| c.starts_with(&format!("{name}=")))
        }

        struct Started {
            state: String,
            nonce: String,
            challenge: String,
            cookie: String,
        }

        async fn begin(s: &TestServer, next: &str) -> Started {
            let res = s.get(&format!("/auth/google/start{next}")).await;
            assert_eq!(res.status_code(), 303);
            let to = reqwest::Url::parse(res.header("location").to_str().unwrap()).unwrap();
            assert_eq!(to.host_str(), Some("accounts.google.com"));
            let q: HashMap<_, _> = to.query_pairs().into_owned().collect();
            assert_eq!(
                q["redirect_uri"],
                "http://localhost:8080/auth/google/callback"
            );
            assert_eq!(q["scope"], "openid email profile");
            assert_eq!(q["code_challenge_method"], "S256");
            let cookie = set_cookie(&res, "sr_google").unwrap();
            assert!(cookie.contains("HttpOnly") && cookie.contains("Path=/auth/google"));
            Started {
                state: q["state"].clone(),
                nonce: q["nonce"].clone(),
                challenge: q["code_challenge"].clone(),
                cookie: cookie.split(';').next().unwrap().to_owned(),
            }
        }

        fn claims(st: &Started, sub: &str, email: &str) -> Value {
            // `unique_email()` is on example.com, so the account is that Workspace domain's.
            json!({"sub": sub, "email": email, "email_verified": true, "nonce": st.nonce,
                "name": "Ann Lee", "hd": "example.com"})
        }

        /// Google's redirect back: (where it lands, the session cookie if one was set).
        async fn finish(
            s: &TestServer,
            state: &str,
            cookie: Option<&str>,
            claims: &Value,
        ) -> (String, Option<String>) {
            let code = URL_SAFE_NO_PAD.encode(claims.to_string());
            let mut req = s.get(&format!("/auth/google/callback?code={code}&state={state}"));
            if let Some(c) = cookie {
                req = req.add_header("cookie", c);
            }
            let res = req.await;
            assert_eq!(res.status_code(), 303);
            let cleared = set_cookie(&res, "sr_google").unwrap();
            assert!(cleared.contains("Max-Age=0"), "the state cookie is cleared");
            let session = set_cookie(&res, "sr_session").map(|c| session(&c));
            (res.header("location").to_str().unwrap().to_owned(), session)
        }

        fn sub() -> String {
            uuid::Uuid::now_v7().simple().to_string()
        }

        #[tokio::test]
        #[serial]
        async fn without_credentials_google_is_off() {
            let (s, _) = server_where(|c| c.google = None).await;
            s.get("/auth/providers")
                .await
                .assert_json(&json!({"google": false}));
            assert_eq!(s.get("/auth/google/start").await.status_code(), 404);
            assert_eq!(s.get("/auth/google/callback").await.status_code(), 404);
        }

        #[tokio::test]
        #[serial]
        async fn a_new_google_account_signs_up_and_lands_on_welcome() {
            let (s, db) = google_server(|_| {}).await;
            s.get("/auth/providers")
                .await
                .assert_json(&json!({"google": true}));
            let st = begin(&s, "?next=/app/team").await;
            let email = unique_email();
            let id = sub();
            let (to, session) = finish(
                &s,
                &st.state,
                Some(&st.cookie),
                &claims(&st, &id, &email.to_uppercase()),
            )
            .await;
            assert_eq!(to, "http://localhost:3000/welcome?next=%2Fapp%2Fteam");
            let verifier = VERIFIERS.lock().unwrap().last().cloned().unwrap();
            assert_eq!(
                socialreins_core::oauth::pkce_challenge(&verifier),
                st.challenge
            );
            let me = s
                .get("/auth/me")
                .add_header("cookie", session.unwrap())
                .await;
            me.assert_status_ok();
            assert_eq!(me.json::<Value>()["email"], email);
            assert_eq!(me.json::<Value>()["name"], "Ann Lee");
            assert_eq!(user(&db, &email).await.unwrap().google_sub, Some(id));
        }

        #[tokio::test]
        #[serial]
        async fn existing_accounts_match_by_sub_then_by_verified_email() {
            let (s, db) = google_server(|_| {}).await;
            let (email, _) = signed_in(&s, &db).await;
            let id = sub();
            let st = begin(&s, "?next=/app/team?tab=members").await;
            let (to, session) =
                finish(&s, &st.state, Some(&st.cookie), &claims(&st, &id, &email)).await;
            assert_eq!(to, "http://localhost:3000/app/team?tab=members");
            assert!(session.is_some());
            let linked = user(&db, &email).await.unwrap();
            assert_eq!(linked.google_sub.as_deref(), Some(id.as_str()));
            // The Google account's email changed: the linked sub still finds the account.
            let st = begin(&s, "?next=//evil.example").await;
            let (to, session) = finish(
                &s,
                &st.state,
                Some(&st.cookie),
                &claims(&st, &id, &unique_email()),
            )
            .await;
            assert_eq!(
                to, "http://localhost:3000/app",
                "an off-site next falls back to /app"
            );
            let me = s
                .get("/auth/me")
                .add_header("cookie", session.unwrap())
                .await;
            assert_eq!(me.json::<Value>()["id"], json!(linked.id));
            // Another Google account with the same email cannot take it over.
            let st = begin(&s, "").await;
            let (to, session) = finish(
                &s,
                &st.state,
                Some(&st.cookie),
                &claims(&st, &sub(), &email),
            )
            .await;
            assert_eq!(to, "http://localhost:3000/login?error=google_failed");
            assert!(session.is_none());
        }

        #[tokio::test]
        #[serial]
        async fn state_cookie_nonce_and_verified_email_are_required() {
            let (s, db) = google_server(|_| {}).await;
            let fail = "http://localhost:3000/login?error=google_failed";
            let st = begin(&s, "").await;
            let email = unique_email();
            // No cookie, or another browser's cookie: refused, and the state is not used up.
            assert_eq!(
                finish(&s, &st.state, None, &claims(&st, &sub(), &email))
                    .await
                    .0,
                fail
            );
            let other = begin(&s, "").await;
            assert_eq!(
                finish(
                    &s,
                    &st.state,
                    Some(&other.cookie),
                    &claims(&st, &sub(), &email)
                )
                .await
                .0,
                fail
            );
            let mut unverified = claims(&st, &sub(), &email);
            unverified["email_verified"] = json!(false);
            assert_eq!(
                finish(&s, &st.state, Some(&st.cookie), &unverified).await.0,
                fail
            );
            // That attempt used the state: a replay fails even with good claims.
            assert_eq!(
                finish(
                    &s,
                    &st.state,
                    Some(&st.cookie),
                    &claims(&st, &sub(), &email)
                )
                .await
                .0,
                fail
            );
            let st = begin(&s, "").await;
            let mut wrong_nonce = claims(&st, &sub(), &email);
            wrong_nonce["nonce"] = json!("not-the-nonce");
            assert_eq!(
                finish(&s, &st.state, Some(&st.cookie), &wrong_nonce)
                    .await
                    .0,
                fail
            );
            let st = begin(&s, "").await;
            let mut wrong_aud = claims(&st, &sub(), &email);
            wrong_aud["aud"] = json!("someone-else");
            assert_eq!(
                finish(&s, &st.state, Some(&st.cookie), &wrong_aud).await.0,
                fail
            );
            assert!(user(&db, &email).await.is_none(), "no account was made");
        }

        #[tokio::test]
        #[serial]
        async fn only_gmail_or_the_workspace_domain_links_by_email() {
            let (s, db) = google_server(|_| {}).await;
            let fail = "http://localhost:3000/login?error=google_failed";
            let (existing, _) = signed_in(&s, &db).await;
            for (email, hd) in [
                (unique_email(), None),
                (existing.clone(), None),
                (unique_email(), Some("other.example")),
            ] {
                let st = begin(&s, "").await;
                let mut c = claims(&st, &sub(), &email);
                c["hd"] = json!(hd);
                let (to, session) = finish(&s, &st.state, Some(&st.cookie), &c).await;
                assert_eq!(to, fail, "{email} {hd:?}");
                assert!(session.is_none());
            }
            assert!(
                user(&db, &existing).await.unwrap().google_sub.is_none(),
                "not linked"
            );
            // A Gmail address needs no hd; a new one signs up and lands on welcome.
            let gmail = format!("{}@gmail.com", sub());
            let st = begin(&s, "").await;
            let mut c = claims(&st, &sub(), &gmail);
            c["hd"] = Value::Null;
            let (to, _) = finish(&s, &st.state, Some(&st.cookie), &c).await;
            assert_eq!(to, "http://localhost:3000/welcome");
            assert!(user(&db, &gmail).await.is_some());
        }

        #[tokio::test]
        #[serial]
        async fn a_fresh_sign_in_links_any_address_to_itself() {
            let (s, db) = google_server(|_| {}).await;
            let (email, session) = signed_in(&s, &db).await;
            let id = sub();
            let both = |st: &Started| format!("{}; {session}", st.cookie);
            let st = begin(&s, "?next=/app/security").await;
            let mut c = claims(&st, &id, &email);
            c["hd"] = Value::Null;
            let (to, _) = finish(&s, &st.state, Some(&both(&st)), &c).await;
            assert_eq!(to, "http://localhost:3000/app/security");
            assert_eq!(
                user(&db, &email).await.unwrap().google_sub,
                Some(id.clone())
            );
            // Linked: the sub alone signs in from now on, with no session.
            let st = begin(&s, "").await;
            let mut c = claims(&st, &id, &email);
            c["hd"] = Value::Null;
            assert_eq!(
                finish(&s, &st.state, Some(&st.cookie), &c).await.0,
                "http://localhost:3000/app"
            );
            // An old session does not link.
            let (other, old) = signed_in(&s, &db).await;
            age(&db, &old, Duration::minutes(11)).await;
            let st = begin(&s, "").await;
            let mut c = claims(&st, &sub(), &other);
            c["hd"] = Value::Null;
            let (to, _) = finish(&s, &st.state, Some(&format!("{}; {old}", st.cookie)), &c).await;
            assert_eq!(to, "http://localhost:3000/login?error=google_failed");
            assert!(user(&db, &other).await.unwrap().google_sub.is_none());
        }

        #[tokio::test]
        #[serial]
        async fn owner_email_needs_a_mailbox_google_vouches_for() {
            let owner = unique_email();
            let o = owner.clone();
            let (s, db) = google_server(move |c| {
                c.registration_disabled = true;
                c.owner_email = Some(o);
            })
            .await;
            let st = begin(&s, "").await;
            let mut c = claims(&st, &sub(), &owner);
            c["hd"] = Value::Null;
            let (to, _) = finish(&s, &st.state, Some(&st.cookie), &c).await;
            assert_eq!(to, "http://localhost:3000/login?error=google_failed");
            assert!(user(&db, &owner).await.is_none());
            let st = begin(&s, "").await;
            let (to, _) = finish(
                &s,
                &st.state,
                Some(&st.cookie),
                &claims(&st, &sub(), &owner),
            )
            .await;
            assert_eq!(to, "http://localhost:3000/welcome");
            assert!(user(&db, &owner).await.is_some());
        }

        #[tokio::test]
        #[serial]
        async fn closed_registration_refuses_new_google_accounts() {
            let (s, db) = google_server(|c| c.registration_disabled = true).await;
            let st = begin(&s, "").await;
            let email = unique_email();
            let (to, session) = finish(
                &s,
                &st.state,
                Some(&st.cookie),
                &claims(&st, &sub(), &email),
            )
            .await;
            assert_eq!(
                to,
                "http://localhost:3000/login?error=registration_disabled"
            );
            assert!(session.is_none());
            assert!(user(&db, &email).await.is_none());
        }

        #[tokio::test]
        #[serial]
        async fn google_sign_in_shares_the_login_limit() {
            let (s, _) = google_server(|_| {}).await;
            for _ in 0..10 {
                assert_eq!(s.get("/auth/google/start").await.status_code(), 303);
            }
            let res = s.get("/auth/google/start").await;
            assert_eq!(
                res.header("location").to_str().unwrap(),
                "http://localhost:3000/login?error=google_failed"
            );
        }
    }

    mod passkeys {
        use super::*;
        use webauthn_authenticator_rs::{
            prelude::Url, softpasskey::SoftPasskey, WebauthnAuthenticator,
        };

        async fn post(s: &TestServer, path: &str, cookie: Option<&str>, body: Value) -> Value {
            let mut req = s.post(path).add_header("origin", WEB).json(&body);
            if let Some(c) = cookie {
                req = req.add_header("cookie", c);
            }
            let res = req.await;
            let status = res.status_code().as_u16();
            json!({"status": status, "body": res.json::<Value>(), "cookie":
                res.maybe_header("set-cookie").map(|c| session(c.to_str().unwrap()))})
        }

        #[tokio::test]
        #[serial]
        async fn start_endpoints_return_webauthn_options() {
            let (s, db) = server_where(|_| {}).await;
            let (email, cookie) = signed_in(&s, &db).await;
            let reg = post(
                &s,
                "/auth/passkeys/register/start",
                Some(&cookie),
                json!({}),
            )
            .await;
            assert_eq!(reg["status"], 200, "{reg}");
            let pk = &reg["body"]["options"]["publicKey"];
            assert!(reg["body"]["challenge_id"].is_string());
            assert!(pk["challenge"].as_str().unwrap().len() >= 32);
            assert_eq!(pk["rp"]["id"], "localhost");
            assert_eq!(pk["user"]["name"], email);
            assert_eq!(pk["authenticatorSelection"]["residentKey"], "required");
            assert_eq!(pk["authenticatorSelection"]["userVerification"], "required");

            let login = post(&s, "/auth/passkeys/login/start", None, Value::Null).await;
            assert_eq!(login["status"], 200, "{login}");
            let pk = &login["body"]["options"]["publicKey"];
            assert_eq!(pk["rpId"], "localhost");
            assert!(pk["challenge"].is_string());
            assert!(login["body"]["options"].get("mediation").is_none());

            let signed_out = s
                .post("/auth/passkeys/register/start")
                .add_header("origin", WEB)
                .json(&json!({}))
                .await;
            assert_eq!(signed_out.status_code(), 401);
        }

        /// The full ceremony against a software authenticator. It cannot store discoverable
        /// credentials, so the test plays the browser's part: it relaxes `residentKey` on the way
        /// in, and on sign-in passes the credential id and returns the user handle.
        #[tokio::test]
        #[serial]
        async fn register_sign_in_and_replays() {
            let (s, db) = server_where(|_| {}).await;
            let (_, cookie) = signed_in(&s, &db).await;
            let (_, stranger) = signed_in(&s, &db).await;
            let origin = Url::parse(WEB).unwrap();
            let mut authenticator = WebauthnAuthenticator::new(SoftPasskey::new(true));

            let start = post(
                &s,
                "/auth/passkeys/register/start",
                Some(&cookie),
                json!({}),
            )
            .await;
            let challenge_id = start["body"]["challenge_id"].clone();
            let mut options = start["body"]["options"].clone();
            options["publicKey"]["authenticatorSelection"]["residentKey"] = json!("discouraged");
            options["publicKey"]["authenticatorSelection"]["requireResidentKey"] = json!(false);
            let credential = authenticator
                .do_registration(origin.clone(), serde_json::from_value(options).unwrap())
                .unwrap();
            let finish = json!({"challenge_id": challenge_id, "credential": credential,
                "name": "Laptop"});
            // Someone else's session cannot finish this user's ceremony, and that uses it up.
            let theirs = post(
                &s,
                "/auth/passkeys/register/finish",
                Some(&stranger),
                finish.clone(),
            )
            .await;
            assert_eq!(theirs["status"], 400);
            assert_eq!(theirs["body"], json!({"error": "invalid_challenge"}));

            let start = post(
                &s,
                "/auth/passkeys/register/start",
                Some(&cookie),
                json!({}),
            )
            .await;
            let mut options = start["body"]["options"].clone();
            options["publicKey"]["authenticatorSelection"]["residentKey"] = json!("discouraged");
            options["publicKey"]["authenticatorSelection"]["requireResidentKey"] = json!(false);
            let credential = authenticator
                .do_registration(origin.clone(), serde_json::from_value(options).unwrap())
                .unwrap();
            let finish = json!({"challenge_id": start["body"]["challenge_id"],
                "credential": credential, "name": "Laptop"});
            let made = post(
                &s,
                "/auth/passkeys/register/finish",
                Some(&cookie),
                finish.clone(),
            )
            .await;
            assert_eq!(made["status"], 201, "{made}");
            assert_eq!(made["body"]["name"], "Laptop");
            let replay = post(&s, "/auth/passkeys/register/finish", Some(&cookie), finish).await;
            assert_eq!(replay["status"], 400);
            assert_eq!(replay["body"], json!({"error": "invalid_challenge"}));
            let list = s.get("/auth/passkeys").add_header("cookie", &cookie).await;
            assert_eq!(list.json::<Value>().as_array().unwrap().len(), 1);

            let user_id = s
                .get("/auth/me")
                .add_header("cookie", &cookie)
                .await
                .json::<Value>()["id"]
                .as_str()
                .unwrap()
                .parse::<uuid::Uuid>()
                .unwrap();
            let start = post(&s, "/auth/passkeys/login/start", None, Value::Null).await;
            let mut options = start["body"]["options"].clone();
            options["publicKey"]["allowCredentials"] =
                json!([{"type": "public-key", "id": credential.raw_id}]);
            let mut assertion = authenticator
                .do_authentication(origin, serde_json::from_value(options).unwrap())
                .unwrap();
            assertion.response.user_handle = Some(user_id.as_bytes().to_vec().into());
            let finish = json!({"challenge_id": start["body"]["challenge_id"],
                "credential": assertion});
            let ok = post(&s, "/auth/passkeys/login/finish", None, finish.clone()).await;
            assert_eq!(ok["status"], 200, "{ok}");
            let fresh = ok["cookie"].as_str().unwrap();
            s.get("/auth/me")
                .add_header("cookie", fresh)
                .await
                .assert_status_ok();
            let replay = post(&s, "/auth/passkeys/login/finish", None, finish).await;
            assert_eq!(replay["status"], 400);
            assert_eq!(replay["body"], json!({"error": "invalid_challenge"}));
            let list = s.get("/auth/passkeys").add_header("cookie", &cookie).await;
            assert!(list.json::<Value>()[0]["last_used_at"].is_string());

            // The conditional counter update reads this path; it must exist in what we store.
            let made_id: uuid::Uuid = made["body"]["id"].as_str().unwrap().parse().unwrap();
            let stored = socialreins_brain::models::passkeys::Entity::find_by_id(made_id)
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            assert!(stored.credential["cred"]["counter"].is_number());

            let id = made["body"]["id"].as_str().unwrap();
            let remove = |c: &str| {
                s.delete(&format!("/auth/passkeys/{id}"))
                    .add_header("origin", WEB)
                    .add_header("cookie", c)
            };
            let res = remove(&stranger).await;
            assert_eq!(res.status_code(), 404);
            res.assert_json(&json!({"error": "not_found"}));
            // A stolen old cookie cannot remove it.
            age(&db, &cookie, Duration::minutes(11)).await;
            let res = remove(&cookie).await;
            assert_eq!(res.status_code(), 403);
            res.assert_json(&json!({"error": "reauth_required"}));
            age(&db, &cookie, Duration::zero()).await;
            assert_eq!(remove(&cookie).await.status_code(), 204);
            assert_eq!(remove(&cookie).await.status_code(), 404);
            // The passkey's session ended with it; the one that deleted it stays.
            let me = |c: &str| s.get("/auth/me").add_header("cookie", c.to_owned());
            assert_eq!(me(fresh).await.status_code(), 401);
            me(&cookie).await.assert_status_ok();
        }

        #[tokio::test]
        #[serial]
        async fn finish_rejects_unknown_challenges_and_bad_credentials() {
            let (s, db) = server_where(|_| {}).await;
            let (_, cookie) = signed_in(&s, &db).await;
            let start = post(&s, "/auth/passkeys/login/start", None, Value::Null).await;
            // A well-formed assertion from a key brain never registered.
            let mut options = start["body"]["options"].clone();
            let mut stray = WebauthnAuthenticator::new(SoftPasskey::new(true));
            let reg_start = post(
                &s,
                "/auth/passkeys/register/start",
                Some(&cookie),
                json!({}),
            )
            .await;
            let mut reg_options = reg_start["body"]["options"].clone();
            reg_options["publicKey"]["authenticatorSelection"]["requireResidentKey"] = json!(false);
            let stray_cred = stray
                .do_registration(
                    Url::parse(WEB).unwrap(),
                    serde_json::from_value(reg_options).unwrap(),
                )
                .unwrap();
            options["publicKey"]["allowCredentials"] =
                json!([{"type": "public-key", "id": stray_cred.raw_id}]);
            let mut assertion = stray
                .do_authentication(
                    Url::parse(WEB).unwrap(),
                    serde_json::from_value(options).unwrap(),
                )
                .unwrap();
            assertion.response.user_handle = Some(uuid::Uuid::now_v7().as_bytes().to_vec().into());
            let unknown = json!({"challenge_id": uuid::Uuid::now_v7(), "credential": assertion});
            let res = post(&s, "/auth/passkeys/login/finish", None, unknown).await;
            assert_eq!(res["status"], 400);
            assert_eq!(res["body"], json!({"error": "invalid_challenge"}));
            let finish = json!({"challenge_id": start["body"]["challenge_id"],
                "credential": assertion});
            let res = post(&s, "/auth/passkeys/login/finish", None, finish).await;
            assert_eq!(res["status"], 401);
            assert_eq!(res["body"], json!({"error": "invalid_passkey"}));
            assert!(res["cookie"].is_null());
        }
    }
}

mod teams {
    use super::*;
    use axum::http::Method;
    use axum_test::TestRequest;
    use chrono::{Duration, Utc};
    use sea_orm::{ActiveValue::Set, DatabaseConnection};
    use socialreins_brain::models::{sessions, social_sets, team_invitations, team_user, users};
    use socialreins_core::token;

    const WEB: &str = "http://localhost:3000";

    async fn server_where(edit: impl FnOnce(&mut Config)) -> (TestServer, DatabaseConnection) {
        let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
        // Lets `sign_in` spread over client IPs, so making many users stays under the link limit.
        config.trust_proxy = true;
        edit(&mut config);
        let db = connect(&config).await.expect("database reachable");
        let app = controllers::router(AppState::new(db.clone(), &config), &config);
        (TestServer::new(app).unwrap(), db)
    }

    /// Signs `email` in by link (creating the account), returning the `sr_session=...` cookie.
    async fn sign_in(s: &TestServer, db: &DatabaseConnection, email: &str) -> Option<String> {
        let secret = token::new();
        let b = token::new().into_bytes();
        let row = socialreins_brain::models::magic_links::ActiveModel {
            token_hash: Set(token::hash(&secret)),
            email: Set(email.into()),
            expires_at: Set((Utc::now() + Duration::minutes(15)).fixed_offset()),
            ..Default::default()
        };
        socialreins_brain::models::magic_links::Entity::insert(row)
            .exec_without_returning(db)
            .await
            .unwrap();
        let res = s
            .post("/auth/magic")
            .add_header("origin", "http://localhost:8080")
            .add_header("x-forwarded-for", format!("10.{}.{}.{}", b[0], b[1], b[2]))
            .form(&[("token", secret.as_str())])
            .await;
        res.maybe_header("set-cookie")
            .map(|c| c.to_str().unwrap().split(';').next().unwrap().to_owned())
    }

    /// A new signed-in user: (email, cookie, user id).
    async fn user(s: &TestServer, db: &DatabaseConnection) -> (String, String, uuid::Uuid) {
        let email = unique_email();
        let cookie = sign_in(s, db, &email).await.unwrap();
        let me = s.get("/auth/me").add_header("cookie", &cookie).await;
        let id = me.json::<Value>()["id"].as_str().unwrap().parse().unwrap();
        (email, cookie, id)
    }

    /// A request from the web app with `cookie`.
    fn req(s: &TestServer, method: Method, path: &str, cookie: &str) -> TestRequest {
        s.method(method, path)
            .add_header("origin", WEB)
            .add_header("cookie", cookie.to_owned())
    }

    async fn current(s: &TestServer, cookie: &str) -> Value {
        let res = s
            .get("/v1/team")
            .add_header("cookie", cookie.to_owned())
            .await;
        res.assert_status_ok();
        res.json()
    }

    async fn memberships(db: &DatabaseConnection, user_id: uuid::Uuid) -> u64 {
        team_user::Entity::find()
            .filter(team_user::Column::UserId.eq(user_id))
            .count(db)
            .await
            .unwrap()
    }

    /// Stores an invitation the way `POST /v1/team/invitations` does and returns its token.
    async fn invitation(
        db: &DatabaseConnection,
        team_id: &str,
        email: &str,
        role: &str,
        inviter: Option<uuid::Uuid>,
        valid_for: Duration,
    ) -> String {
        let secret = token::new();
        let row = team_invitations::ActiveModel {
            id: Set(uuid::Uuid::now_v7()),
            team_id: Set(team_id.parse().unwrap()),
            email: Set(email.into()),
            role: Set(role.into()),
            token_hash: Set(token::hash(&secret)),
            invited_by: Set(inviter),
            expires_at: Set((Utc::now() + valid_for).fixed_offset()),
            ..Default::default()
        };
        team_invitations::Entity::insert(row)
            .exec_without_returning(db)
            .await
            .unwrap();
        secret
    }

    /// A new user who joined `team_id` with `role`: (cookie, user id).
    async fn joined(
        s: &TestServer,
        db: &DatabaseConnection,
        team_id: &str,
        role: &str,
    ) -> (String, uuid::Uuid) {
        let (email, cookie, id) = user(s, db).await;
        let owner = team_user::Entity::find()
            .filter(team_user::Column::TeamId.eq(team_id.parse::<uuid::Uuid>().unwrap()))
            .filter(team_user::Column::Role.eq("owner"))
            .one(db)
            .await
            .unwrap()
            .map(|m| m.user_id);
        let secret = invitation(db, team_id, &email, role, owner, Duration::days(7)).await;
        let res = req(
            s,
            Method::POST,
            &format!("/v1/invitations/{secret}"),
            &cookie,
        )
        .await;
        res.assert_status_ok();
        (cookie, id)
    }

    #[tokio::test]
    #[serial]
    async fn first_sign_in_makes_one_personal_team_with_a_default_set() {
        let (s, db) = server_where(|_| {}).await;
        let email = unique_email();
        // Two links of one new user redeemed at once still make one team.
        let (a, b) = tokio::join!(sign_in(&s, &db, &email), sign_in(&s, &db, &email));
        let cookie = a.or(b).unwrap();
        sign_in(&s, &db, &email).await.unwrap();
        let me = s.get("/auth/me").add_header("cookie", &cookie).await;
        let me = me.json::<Value>();
        let team = &me["current_team"];
        assert_eq!(team["role"], "owner", "{me}");
        let id: uuid::Uuid = me["id"].as_str().unwrap().parse().unwrap();
        assert_eq!(memberships(&db, id).await, 1);
        let row = users::Entity::find_by_id(id)
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.current_team_id.unwrap().to_string(), team["id"]);
        let team_id: uuid::Uuid = team["id"].as_str().unwrap().parse().unwrap();
        let sets = social_sets::Entity::find()
            .filter(social_sets::Column::TeamId.eq(team_id))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(sets.len(), 1);
        assert!(sets[0].is_default && sets[0].name == "Default");
        // The partial unique index allows one Default set per team.
        let second = social_sets::ActiveModel {
            id: Set(uuid::Uuid::now_v7()),
            team_id: Set(team_id),
            name: Set("Other".into()),
            is_default: Set(true),
            ..Default::default()
        };
        assert!(social_sets::Entity::insert(second)
            .exec_without_returning(&db)
            .await
            .is_err());
        let list = s.get("/v1/teams").add_header("cookie", &cookie).await;
        assert_eq!(list.json::<Value>().as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    #[serial]
    async fn create_switch_rename_and_delete() {
        let (s, db) = server_where(|_| {}).await;
        let (_, cookie, id) = user(&s, &db).await;
        let personal = current(&s, &cookie).await;
        let made = req(&s, Method::POST, "/v1/teams", &cookie)
            .json(&json!({"name": "  Acme  "}))
            .await;
        assert_eq!(made.status_code(), 201);
        let acme = made.json::<Value>();
        assert_eq!(
            (acme["name"].clone(), acme["role"].clone()),
            (json!("Acme"), json!("owner"))
        );
        assert_eq!(current(&s, &cookie).await["id"], acme["id"]);
        let me = s.get("/auth/me").add_header("cookie", &cookie).await;
        assert_eq!(me.json::<Value>()["current_team"], acme);
        let acme_id: uuid::Uuid = acme["id"].as_str().unwrap().parse().unwrap();
        let defaults = social_sets::Entity::find()
            .filter(social_sets::Column::TeamId.eq(acme_id))
            .filter(social_sets::Column::IsDefault.eq(true))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(defaults, 1);
        let list = s.get("/v1/teams").add_header("cookie", &cookie).await;
        assert_eq!(list.json::<Value>().as_array().unwrap().len(), 2);

        let blank = req(&s, Method::POST, "/v1/teams", &cookie)
            .json(&json!({"name": " "}))
            .await;
        assert_eq!(blank.status_code(), 422);
        blank.assert_json(&json!({"error": "invalid_name"}));

        let switch = |team: Value| {
            req(&s, Method::PUT, "/v1/current-team", &cookie).json(&json!({"team_id": team}))
        };
        switch(personal["id"].clone()).await.assert_status_ok();
        assert_eq!(current(&s, &cookie).await["id"], personal["id"]);
        let res = switch(json!(uuid::Uuid::now_v7())).await;
        assert_eq!(res.status_code(), 404);
        res.assert_json(&json!({"error": "not_found"}));
        switch(acme["id"].clone()).await.assert_status_ok();

        let rename = req(&s, Method::PATCH, "/v1/team", &cookie)
            .json(&json!({"name": "Acme Co"}))
            .await;
        rename.assert_status_ok();
        assert_eq!(rename.json::<Value>()["name"], "Acme Co");

        // Deleting needs a recent sign-in.
        let hash = token::hash(cookie.trim_start_matches("sr_session="));
        let aged = sessions::ActiveModel {
            token_hash: Set(hash.clone()),
            created_at: Set((Utc::now() - Duration::minutes(11)).fixed_offset()),
            ..Default::default()
        };
        sessions::Entity::update(aged).exec(&db).await.unwrap();
        let delete = || req(&s, Method::DELETE, "/v1/team", &cookie).json(&json!({}));
        let res = delete().await;
        assert_eq!(res.status_code(), 403);
        res.assert_json(&json!({"error": "reauth_required"}));
        let fresh = sessions::ActiveModel {
            token_hash: Set(hash),
            created_at: Set(Utc::now().fixed_offset()),
            ..Default::default()
        };
        sessions::Entity::update(fresh).exec(&db).await.unwrap();
        assert_eq!(delete().await.status_code(), 204);
        assert_eq!(memberships(&db, id).await, 1);
        assert_eq!(current(&s, &cookie).await["id"], personal["id"]);
        let res = delete().await;
        assert_eq!(res.status_code(), 409);
        res.assert_json(&json!({"error": "last_team"}));
    }

    #[tokio::test]
    #[serial]
    async fn invitations_are_emailed_previewed_and_used_once() {
        let (s, db) = server_where(|_| {}).await;
        let (_, owner, owner_id) = user(&s, &db).await;
        let team = current(&s, &owner).await;
        let team_id = team["id"].as_str().unwrap();
        let invitee = unique_email();
        let invite =
            |body: Value| req(&s, Method::POST, "/v1/team/invitations", &owner).json(&body);
        let bad = invite(json!({"email": invitee, "role": "owner"})).await;
        assert_eq!(bad.status_code(), 422);
        bad.assert_json(&json!({"error": "invalid_role"}));
        let made = invite(json!({"email": invitee.to_uppercase(), "role": "admin"})).await;
        assert_eq!(made.status_code(), 201, "{}", made.text());
        let made = made.json::<Value>();
        assert_eq!(
            (made["email"].clone(), made["role"].clone()),
            (json!(invitee), json!("admin"))
        );
        assert!(made.get("token").is_none() && made.get("token_hash").is_none());
        let row = team_invitations::Entity::find()
            .filter(team_invitations::Column::Email.eq(&invitee))
            .one(&db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.token_hash.len(), 64);
        let valid = (row.expires_at - row.created_at).num_minutes();
        assert!((7 * 24 * 60 - 1..=7 * 24 * 60).contains(&valid), "{valid}");
        // Re-inviting replaces the pending invitation instead of adding one.
        let again = invite(json!({"email": invitee, "role": "member"})).await;
        assert_eq!(again.status_code(), 201);
        let again = again.json::<Value>();
        assert_ne!(again["id"], made["id"]);
        let pending = team_invitations::Entity::find()
            .filter(team_invitations::Column::Email.eq(&invitee))
            .all(&db)
            .await
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].role, "member");
        // A third send to the same address the same day is the last.
        let third = invite(json!({"email": invitee, "role": "member"})).await;
        assert_eq!(third.status_code(), 201);
        let made = third.json::<Value>();
        let fourth = invite(json!({"email": invitee, "role": "member"})).await;
        assert_eq!(fourth.status_code(), 429);
        fourth.assert_json(&json!({"error": "rate_limited"}));
        let listed = s
            .get("/v1/team/invitations")
            .add_header("cookie", &owner)
            .await;
        assert_eq!(listed.json::<Value>()[0]["id"], made["id"]);

        // The emailed token is not observable here, so accept one stored the same way.
        let (invited_email, invited, invited_id) = user(&s, &db).await;
        let secret = invitation(
            &db,
            team_id,
            &invited_email,
            "admin",
            Some(owner_id),
            Duration::days(7),
        )
        .await;
        let preview = s.get(&format!("/v1/invitations/{secret}")).await;
        preview.assert_status_ok();
        let preview = preview.json::<Value>();
        assert_eq!(preview["team"], team["name"]);
        assert_eq!(preview["role"], "admin");
        assert_eq!(preview["email"], invited_email);
        assert!(preview["invited_by"].is_string() && preview["expires_at"].is_string());
        for junk in ["nope", &token::new()] {
            let res = s.get(&format!("/v1/invitations/{junk}")).await;
            assert_eq!(res.status_code(), 404);
        }
        let expired = invitation(
            &db,
            team_id,
            &unique_email(),
            "member",
            None,
            Duration::minutes(-1),
        )
        .await;
        assert_eq!(
            s.get(&format!("/v1/invitations/{expired}"))
                .await
                .status_code(),
            404
        );

        let accept = |cookie: &str| {
            req(
                &s,
                Method::POST,
                &format!("/v1/invitations/{secret}"),
                cookie,
            )
        };
        let (_, stranger, _) = user(&s, &db).await;
        let res = accept(&stranger).await;
        assert_eq!(res.status_code(), 403);
        res.assert_json(&json!({"error": "email_mismatch"}));
        let ok = accept(&invited).await;
        ok.assert_status_ok();
        assert_eq!(
            ok.json::<Value>(),
            json!({"id": team["id"], "name": team["name"], "role": "admin"})
        );
        assert_eq!(current(&s, &invited).await["id"], team["id"]);
        assert_eq!(memberships(&db, invited_id).await, 2);
        assert_eq!(accept(&invited).await.status_code(), 404, "used once");
        let members = s.get("/v1/team/members").add_header("cookie", &owner).await;
        let members = members.json::<Value>();
        assert_eq!(members.as_array().unwrap().len(), 2);
        assert_eq!(members[1]["email"], invited_email);
        assert_eq!(members[1]["role"], "admin");

        let joined = invite(json!({"email": invited_email, "role": "member"})).await;
        assert_eq!(joined.status_code(), 409);
        joined.assert_json(&json!({"error": "already_member"}));
        let id = made["id"].as_str().unwrap();
        let revoke = |cookie: &str| {
            req(
                &s,
                Method::DELETE,
                &format!("/v1/team/invitations/{id}"),
                cookie,
            )
        };
        assert_eq!(
            revoke(&stranger).await.status_code(),
            404,
            "another team's invitation"
        );
        assert_eq!(revoke(&owner).await.status_code(), 204);
        assert_eq!(revoke(&owner).await.status_code(), 404);
    }

    #[tokio::test]
    #[serial]
    async fn roles_and_the_last_owner() {
        let (s, db) = server_where(|_| {}).await;
        let (_, owner, owner_id) = user(&s, &db).await;
        let team = current(&s, &owner).await;
        let team_id = team["id"].as_str().unwrap();
        let (admin, admin_id) = joined(&s, &db, team_id, "admin").await;
        let (member, member_id) = joined(&s, &db, team_id, "member").await;
        let set = |cookie: &str, who: uuid::Uuid, role: &str| {
            req(
                &s,
                Method::PATCH,
                &format!("/v1/team/members/{who}"),
                cookie,
            )
            .json(&json!({"role": role}))
        };
        let remove = |cookie: &str, who: uuid::Uuid| {
            req(
                &s,
                Method::DELETE,
                &format!("/v1/team/members/{who}"),
                cookie,
            )
        };
        let status = |r: axum_test::TestResponse| r.status_code().as_u16();

        assert_eq!(status(set(&member, admin_id, "member").await), 403);
        assert_eq!(
            status(set(&admin, member_id, "owner").await),
            403,
            "admins cannot grant owner"
        );
        assert_eq!(status(set(&admin, owner_id, "member").await), 403);
        assert_eq!(status(remove(&admin, owner_id).await), 403);
        assert_eq!(status(set(&admin, member_id, "boss").await), 422);
        let promoted = set(&admin, member_id, "admin").await;
        promoted.assert_status_ok();
        assert_eq!(promoted.json::<Value>()["role"], "admin");
        assert_eq!(status(set(&admin, member_id, "member").await), 200);

        let last = set(&owner, owner_id, "admin").await;
        assert_eq!(last.status_code(), 409);
        last.assert_json(&json!({"error": "last_owner"}));
        assert_eq!(
            status(remove(&owner, owner_id).await),
            409,
            "last owner cannot leave"
        );
        assert_eq!(status(set(&owner, admin_id, "owner").await), 200);
        assert_eq!(
            status(remove(&owner, owner_id).await),
            204,
            "a second owner lets them leave"
        );
        assert_ne!(current(&s, &owner).await["id"], team["id"]);

        // Outside the team everything is 404.
        assert_eq!(status(set(&owner, member_id, "admin").await), 404);
        let (_, stranger, stranger_id) = user(&s, &db).await;
        assert_eq!(status(set(&admin, stranger_id, "admin").await), 404);
        assert_eq!(status(remove(&stranger, member_id).await), 404);

        // The member leaves on their own; the new owner removes nobody else's way.
        assert_eq!(status(remove(&member, member_id).await), 204);
        let members = s.get("/v1/team/members").add_header("cookie", &admin).await;
        assert_eq!(members.json::<Value>().as_array().unwrap().len(), 1);
        let invite = req(&s, Method::POST, "/v1/team/invitations", &member)
            .json(&json!({"email": unique_email(), "role": "member"}))
            .await;
        assert_eq!(
            invite.status_code(),
            201,
            "back in their personal team, as owner"
        );
    }

    #[tokio::test]
    #[serial]
    async fn members_cannot_manage_the_team() {
        let (s, db) = server_where(|_| {}).await;
        let (_, owner, _) = user(&s, &db).await;
        let team_id = current(&s, &owner).await["id"].as_str().unwrap().to_owned();
        let (member, _) = joined(&s, &db, &team_id, "member").await;
        let calls = [
            req(&s, Method::PATCH, "/v1/team", &member).json(&json!({"name": "Mine"})),
            req(&s, Method::DELETE, "/v1/team", &member).json(&json!({})),
            req(&s, Method::POST, "/v1/team/invitations", &member)
                .json(&json!({"email": unique_email(), "role": "member"})),
            req(&s, Method::GET, "/v1/team/invitations", &member),
        ];
        for call in calls {
            let res = call.await;
            assert_eq!(res.status_code(), 403);
            res.assert_json(&json!({"error": "forbidden"}));
        }
        let members = s
            .get("/v1/team/members")
            .add_header("cookie", &member)
            .await;
        members.assert_status_ok();
    }

    #[tokio::test]
    #[serial]
    async fn privilege_changes_need_a_fresh_sign_in() {
        let (s, db) = server_where(|_| {}).await;
        let (_, owner, _) = user(&s, &db).await;
        let team_id = current(&s, &owner).await["id"].as_str().unwrap().to_owned();
        let (admin, _) = joined(&s, &db, &team_id, "admin").await;
        let (_, member_id) = joined(&s, &db, &team_id, "member").await;
        let stale = |cookie: String| {
            let db = db.clone();
            async move {
                let row = sessions::ActiveModel {
                    token_hash: Set(token::hash(cookie.trim_start_matches("sr_session="))),
                    created_at: Set((Utc::now() - Duration::minutes(11)).fixed_offset()),
                    ..Default::default()
                };
                sessions::Entity::update(row).exec(&db).await.unwrap();
            }
        };
        stale(owner.clone()).await;
        let path = format!("/v1/team/members/{member_id}");
        let calls = [
            req(&s, Method::PATCH, &path, &owner).json(&json!({"role": "admin"})),
            req(&s, Method::DELETE, &path, &owner),
            req(&s, Method::POST, "/v1/team/invitations", &owner)
                .json(&json!({"email": unique_email(), "role": "admin"})),
        ];
        for call in calls {
            let res = call.await;
            assert_eq!(res.status_code(), 403);
            res.assert_json(&json!({"error": "reauth_required"}));
        }
        // Inviting a member, or leaving, needs no fresh sign-in.
        let res = req(&s, Method::POST, "/v1/team/invitations", &owner)
            .json(&json!({"email": unique_email(), "role": "member"}))
            .await;
        assert_eq!(res.status_code(), 201);
        stale(admin.clone()).await;
        let me: Value = s.get("/auth/me").add_header("cookie", &admin).await.json();
        let leave = format!("/v1/team/members/{}", me["id"].as_str().unwrap());
        assert_eq!(
            req(&s, Method::DELETE, &leave, &admin).await.status_code(),
            204
        );
    }

    #[tokio::test]
    #[serial]
    async fn team_endpoints_need_a_session_and_the_web_origin() {
        let (s, db) = server_where(|_| {}).await;
        let (_, cookie, _) = user(&s, &db).await;
        for path in [
            "/v1/teams",
            "/v1/team",
            "/v1/team/members",
            "/v1/team/invitations",
        ] {
            assert_eq!(s.get(path).await.status_code(), 401, "{path}");
        }
        let id = uuid::Uuid::now_v7();
        let calls = [
            (Method::POST, "/v1/teams".to_owned()),
            (Method::PUT, "/v1/current-team".to_owned()),
            (Method::PATCH, "/v1/team".to_owned()),
            (Method::DELETE, "/v1/team".to_owned()),
            (Method::PATCH, format!("/v1/team/members/{id}")),
            (Method::DELETE, format!("/v1/team/members/{id}")),
            (Method::POST, "/v1/team/invitations".to_owned()),
            (Method::DELETE, format!("/v1/team/invitations/{id}")),
            (Method::POST, format!("/v1/invitations/{}", token::new())),
        ];
        for (method, path) in calls {
            let res = s
                .method(method, &path)
                .add_header("origin", "https://evil.example")
                .add_header("cookie", &cookie)
                .json(&json!({}))
                .await;
            assert_eq!(res.status_code(), 403, "{path}");
            res.assert_json(&json!({"error": "forbidden_origin"}));
        }
        let preflight = s
            .method(Method::OPTIONS, "/v1/team")
            .add_header("origin", WEB)
            .add_header("access-control-request-method", "PATCH")
            .await;
        assert!(preflight
            .header("access-control-allow-methods")
            .to_str()
            .unwrap()
            .contains("PATCH"));
    }

    #[tokio::test]
    #[serial]
    async fn invitations_are_rate_limited_per_user() {
        let (s, db) = server_where(|_| {}).await;
        let (_, cookie, _) = user(&s, &db).await;
        let invite = || {
            req(&s, Method::POST, "/v1/team/invitations", &cookie)
                .json(&json!({"email": unique_email(), "role": "member"}))
        };
        for _ in 0..10 {
            assert_eq!(invite().await.status_code(), 201);
        }
        let res = invite().await;
        assert_eq!(res.status_code(), 429);
        res.assert_json(&json!({"error": "rate_limited"}));
    }

    #[tokio::test]
    #[serial]
    async fn closed_registration_still_admits_invited_emails() {
        let (open, db) = server_where(|_| {}).await;
        let (_, owner, owner_id) = user(&open, &db).await;
        let team_id = current(&open, &owner).await["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let (s, _) = server_where(|c| c.registration_disabled = true).await;
        assert!(sign_in(&s, &db, &unique_email()).await.is_none());
        let invited = unique_email();
        let secret = invitation(
            &db,
            &team_id,
            &invited,
            "member",
            Some(owner_id),
            Duration::days(7),
        )
        .await;
        let cookie = sign_in(&s, &db, &invited)
            .await
            .expect("invited email signs up");
        let res = req(
            &s,
            Method::POST,
            &format!("/v1/invitations/{secret}"),
            &cookie,
        )
        .await;
        res.assert_status_ok();
        assert_eq!(res.json::<Value>()["role"], "member");
    }

    async fn pending(db: &DatabaseConnection, inviter: uuid::Uuid) -> u64 {
        team_invitations::Entity::find()
            .filter(team_invitations::Column::InvitedBy.eq(inviter))
            .count(db)
            .await
            .unwrap()
    }

    #[tokio::test]
    #[serial]
    async fn invitations_die_with_the_inviters_rights() {
        let (s, db) = server_where(|_| {}).await;
        let (_, owner, _) = user(&s, &db).await;
        let team_id = current(&s, &owner).await["id"].as_str().unwrap().to_owned();
        let invite = |cookie: &str| {
            req(&s, Method::POST, "/v1/team/invitations", cookie)
                .json(&json!({"email": unique_email(), "role": "member"}))
        };
        // Demoted below admin, or removed: their pending invitations go in the same change.
        let (admin, admin_id) = joined(&s, &db, &team_id, "admin").await;
        assert_eq!(invite(&admin).await.status_code(), 201);
        let demote = req(
            &s,
            Method::PATCH,
            &format!("/v1/team/members/{admin_id}"),
            &owner,
        )
        .json(&json!({"role": "member"}));
        demote.await.assert_status_ok();
        assert_eq!(pending(&db, admin_id).await, 0);
        let (other, other_id) = joined(&s, &db, &team_id, "admin").await;
        assert_eq!(invite(&other).await.status_code(), 201);
        let removal = req(
            &s,
            Method::DELETE,
            &format!("/v1/team/members/{other_id}"),
            &owner,
        );
        assert_eq!(removal.await.status_code(), 204);
        assert_eq!(pending(&db, other_id).await, 0);
        // A promotion keeps them.
        let (third, third_id) = joined(&s, &db, &team_id, "admin").await;
        assert_eq!(invite(&third).await.status_code(), 201);
        req(
            &s,
            Method::PATCH,
            &format!("/v1/team/members/{third_id}"),
            &owner,
        )
        .json(&json!({"role": "owner"}))
        .await
        .assert_status_ok();
        assert_eq!(pending(&db, third_id).await, 1);

        // An inviter who lost the right some other way: the link is dead and the row goes.
        let (_, stale_id) = joined(&s, &db, &team_id, "admin").await;
        let (email, cookie, _) = user(&s, &db).await;
        let secret = invitation(
            &db,
            &team_id,
            &email,
            "admin",
            Some(stale_id),
            Duration::days(7),
        )
        .await;
        let row = team_user::ActiveModel {
            team_id: Set(team_id.parse().unwrap()),
            user_id: Set(stale_id),
            role: Set("member".into()),
            ..Default::default()
        };
        team_user::Entity::update(row).exec(&db).await.unwrap();
        let path = format!("/v1/invitations/{secret}");
        assert_eq!(
            req(&s, Method::POST, &path, &cookie).await.status_code(),
            404
        );
        assert_eq!(pending(&db, stale_id).await, 0);
        assert_eq!(s.get(&path).await.status_code(), 404);
        // No inviter left at all is the same.
        let orphan = invitation(&db, &team_id, &email, "member", None, Duration::days(7)).await;
        let path = format!("/v1/invitations/{orphan}");
        assert_eq!(s.get(&path).await.status_code(), 404);
    }

    #[tokio::test]
    #[serial]
    async fn a_team_sends_at_most_50_invitations_a_day() {
        let (s, db) = server_where(|_| {}).await;
        let (_, owner, _) = user(&s, &db).await;
        let team_id = current(&s, &owner).await["id"].as_str().unwrap().to_owned();
        let mut admins = vec![owner];
        for _ in 0..4 {
            admins.push(joined(&s, &db, &team_id, "admin").await.0);
        }
        // Five senders at 10 a minute each fill the team's 50.
        for cookie in &admins {
            for _ in 0..10 {
                let res = req(&s, Method::POST, "/v1/team/invitations", cookie)
                    .json(&json!({"email": unique_email(), "role": "member"}))
                    .await;
                assert_eq!(res.status_code(), 201);
            }
        }
        let extra = joined(&s, &db, &team_id, "admin").await.0;
        let res = req(&s, Method::POST, "/v1/team/invitations", &extra)
            .json(&json!({"email": unique_email(), "role": "member"}))
            .await;
        assert_eq!(res.status_code(), 429);
    }

    #[tokio::test]
    #[serial]
    async fn daily_invitation_caps_span_teams_and_restarts() {
        let (s, db) = server_where(|_| {}).await;
        let (_, a, _) = user(&s, &db).await;
        let (_, b, _) = user(&s, &db).await;
        let invite = |s: &TestServer, cookie: &str, email: &str| {
            req(s, Method::POST, "/v1/team/invitations", cookie)
                .json(&json!({"email": email, "role": "member"}))
        };
        // One address: 20 a day across all teams, however many teams ask.
        let target = unique_email();
        assert_eq!(invite(&s, &a, &target).await.status_code(), 201);
        assert_eq!(invite(&s, &b, &target).await.status_code(), 201);
        for _ in 0..18 {
            let (_, other, _) = user(&s, &db).await;
            assert_eq!(invite(&s, &other, &target).await.status_code(), 201);
        }
        let (_, late, _) = user(&s, &db).await;
        let res = invite(&s, &late, &target).await;
        assert_eq!(res.status_code(), 429);
        res.assert_json(&json!({"error": "rate_limited"}));

        // One inviter: 50 a day, across teams and across fresh servers (fresh memory limiters).
        let mut sent = 1;
        for round in 0..5 {
            let (s, _) = server_where(|_| {}).await;
            if round == 2 {
                req(&s, Method::POST, "/v1/teams", &a)
                    .json(&json!({"name": "Other"}))
                    .await
                    .assert_status(axum::http::StatusCode::CREATED);
            }
            for _ in 0..10 {
                if sent == 50 {
                    break;
                }
                assert_eq!(invite(&s, &a, &unique_email()).await.status_code(), 201);
                sent += 1;
            }
        }
        let (s, _) = server_where(|_| {}).await;
        assert_eq!(invite(&s, &a, &unique_email()).await.status_code(), 429);
    }

    #[tokio::test]
    #[serial]
    async fn a_user_creates_at_most_10_teams_a_day() {
        let (s, db) = server_where(|_| {}).await;
        let (_, cookie, _) = user(&s, &db).await;
        let create = |s: &TestServer| {
            req(s, Method::POST, "/v1/teams", &cookie).json(&json!({"name": "Spam"}))
        };
        for _ in 0..10 {
            assert_eq!(create(&s).await.status_code(), 201);
        }
        // Deleting a team does not give its slot back, and a restart does not reset the count.
        req(&s, Method::DELETE, "/v1/team", &cookie)
            .json(&json!({}))
            .await
            .assert_status(axum::http::StatusCode::NO_CONTENT);
        let (s, _) = server_where(|_| {}).await;
        let res = create(&s).await;
        assert_eq!(res.status_code(), 429);
        res.assert_json(&json!({"error": "rate_limited"}));
    }

    #[tokio::test]
    #[serial]
    async fn two_owners_demoting_each_other_at_once_leave_one_owner() {
        let (s, db) = server_where(|_| {}).await;
        let (_, a, a_id) = user(&s, &db).await;
        let team_id = current(&s, &a).await["id"].as_str().unwrap().to_owned();
        let (b, b_id) = joined(&s, &db, &team_id, "admin").await;
        req(&s, Method::PATCH, &format!("/v1/team/members/{b_id}"), &a)
            .json(&json!({"role": "owner"}))
            .await
            .assert_status_ok();
        let demote = |cookie: &str, who: uuid::Uuid| {
            req(
                &s,
                Method::PATCH,
                &format!("/v1/team/members/{who}"),
                cookie,
            )
            .json(&json!({"role": "admin"}))
        };
        let (x, y) = tokio::join!(demote(&a, b_id), demote(&b, a_id));
        let codes = [x.status_code().as_u16(), y.status_code().as_u16()];
        assert_eq!(codes.iter().filter(|&&c| c == 200).count(), 1, "{codes:?}");
        let owners = team_user::Entity::find()
            .filter(team_user::Column::TeamId.eq(team_id.parse::<uuid::Uuid>().unwrap()))
            .filter(team_user::Column::Role.eq("owner"))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(owners, 1);
    }

    #[tokio::test]
    #[serial]
    async fn an_invitation_is_accepted_once_under_concurrency() {
        let (s, db) = server_where(|_| {}).await;
        let (_, owner, owner_id) = user(&s, &db).await;
        let team_id = current(&s, &owner).await["id"].as_str().unwrap().to_owned();
        let (email, cookie, _) = user(&s, &db).await;
        let secret = invitation(
            &db,
            &team_id,
            &email,
            "member",
            Some(owner_id),
            Duration::days(7),
        )
        .await;
        let path = format!("/v1/invitations/{secret}");
        let (x, y) = tokio::join!(
            req(&s, Method::POST, &path, &cookie),
            req(&s, Method::POST, &path, &cookie)
        );
        let mut codes = [x.status_code().as_u16(), y.status_code().as_u16()];
        codes.sort();
        assert_eq!(codes, [200, 404]);
    }

    #[tokio::test]
    #[serial]
    async fn concurrent_invitations_of_one_address_leave_one() {
        let (s, db) = server_where(|_| {}).await;
        let (_, owner, _) = user(&s, &db).await;
        let email = unique_email();
        let invite = || {
            req(&s, Method::POST, "/v1/team/invitations", &owner)
                .json(&json!({"email": email, "role": "member"}))
        };
        let (x, y) = tokio::join!(invite(), invite());
        let codes = [x.status_code().as_u16(), y.status_code().as_u16()];
        assert!(codes.contains(&201), "{codes:?}");
        for res in [x, y] {
            if res.status_code() == 409 {
                res.assert_json(&json!({"error": "already_invited"}));
            } else {
                assert_eq!(res.status_code(), 201);
            }
        }
        let rows = team_invitations::Entity::find()
            .filter(team_invitations::Column::Email.eq(&email))
            .count(&db)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    #[serial]
    async fn a_demoted_owner_cannot_delete_the_team() {
        let (s, db) = server_where(|_| {}).await;
        let (_, a, _) = user(&s, &db).await;
        let team_id = current(&s, &a).await["id"].as_str().unwrap().to_owned();
        let (b, b_id) = joined(&s, &db, &team_id, "admin").await;
        let set = |role: &str| {
            req(&s, Method::PATCH, &format!("/v1/team/members/{b_id}"), &a)
                .json(&json!({"role": role}))
        };
        let delete = || req(&s, Method::DELETE, "/v1/team", &b).json(&json!({}));
        set("owner").await.assert_status_ok();
        set("admin").await.assert_status_ok();
        let res = delete().await;
        assert_eq!(res.status_code(), 403);
        res.assert_json(&json!({"error": "forbidden"}));
        // Raced: the demotion and the delete never both win.
        set("owner").await.assert_status_ok();
        let (demoted, deleted) = tokio::join!(set("admin"), delete());
        let both = (
            demoted.status_code().as_u16(),
            deleted.status_code().as_u16(),
        );
        assert!(matches!(both, (200, 403) | (404, 204)), "{both:?}");
    }

    #[tokio::test]
    #[serial]
    async fn concurrent_team_creations_stop_exactly_at_the_cap() {
        let (s, db) = server_where(|_| {}).await;
        let (_, cookie, id) = user(&s, &db).await;
        // The test client is not Send, so the 15 requests interleave on one local task set.
        let local = tokio::task::LocalSet::new();
        let codes = local
            .run_until(async {
                let mut set = tokio::task::JoinSet::new();
                for _ in 0..15 {
                    let call =
                        req(&s, Method::POST, "/v1/teams", &cookie).json(&json!({"name": "Race"}));
                    set.spawn_local(async move { call.await.status_code().as_u16() });
                }
                set.join_all().await
            })
            .await;
        assert_eq!(codes.iter().filter(|&&c| c == 201).count(), 10, "{codes:?}");
        assert_eq!(codes.iter().filter(|&&c| c == 429).count(), 5, "{codes:?}");
        assert_eq!(memberships(&db, id).await, 11, "personal team plus 10");
    }

    #[tokio::test]
    #[serial]
    async fn team_names_are_bounded() {
        let (s, db) = server_where(|_| {}).await;
        let (_, cookie, _) = user(&s, &db).await;
        for bad in ["a".repeat(61), "Acme\nhttps://evil".into()] {
            let res = req(&s, Method::POST, "/v1/teams", &cookie)
                .json(&json!({"name": bad}))
                .await;
            assert_eq!(res.status_code(), 422);
            let res = req(&s, Method::PATCH, "/v1/team", &cookie)
                .json(&json!({"name": bad}))
                .await;
            assert_eq!(res.status_code(), 422);
        }
    }

    #[tokio::test]
    #[serial]
    async fn openapi_documents_the_team_routes() {
        let (s, _) = server_where(|_| {}).await;
        let doc = s.get("/openapi.json").await.json::<Value>();
        for (path, method) in [
            ("/teams", "post"),
            ("/teams", "get"),
            ("/current-team", "put"),
            ("/team", "delete"),
            ("/team/members/{user_id}", "patch"),
            ("/team/invitations/{id}", "delete"),
            ("/invitations/{token}", "post"),
        ] {
            assert!(doc["paths"][path][method].is_object(), "{path} {method}");
        }
    }

    /// `/v1/connections`, against a mock Bluesky (AppView, PLC directory and PDS in one local
    /// server), so tests never reach the real network.
    mod connections {
        use super::*;
        use axum::{
            extract::{Path, Query, State},
            routing::{get, post},
            Json, Router,
        };
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        use socialreins_brain::models::{social_account_social_set, social_accounts};
        use std::collections::HashMap;

        const APP_PASSWORD: &str = "abcd-efgh-ijkl-mnop";
        const NEW_APP_PASSWORD: &str = "qrst-uvwx-yzab-cdef";
        const ACCOUNT_PASSWORD: &str = "the-account-password";

        /// An app password made with DM access.
        const DM_APP_PASSWORD: &str = "dmdm-efgh-ijkl-mnop";

        /// A token naming its account in `sub`, so the mock can answer per account.
        fn jwt_for(scope: &str, did: &str) -> String {
            let claims = json!({ "scope": scope, "sub": did }).to_string();
            format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims))
        }

        /// Handles `*.test` resolve to `did:plc:<first label>`, except `nobody.test`.
        async fn mock_bluesky() -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let resolve = |Query(q): Query<HashMap<String, String>>| async move {
                let h = q.get("handle").cloned().unwrap_or_default();
                match h.strip_suffix(".test") {
                    Some("big") => (
                        axum::http::StatusCode::OK,
                        Json(json!({"did": "did:plc:big", "pad": "x".repeat(100_000)})),
                    ),
                    Some("down") => (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({"error": "Unavailable"})),
                    ),
                    Some(label) if label != "nobody" => (
                        axum::http::StatusCode::OK,
                        Json(json!({"did": format!("did:plc:{label}")})),
                    ),
                    _ => (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(json!({"error": "InvalidRequest"})),
                    ),
                }
            };
            // `did:plc:rebind` names its PDS `localhost`, which the resolver must refuse.
            let doc = |State(base): State<String>, Path(did): Path<String>| async move {
                let base = if did == "did:plc:rebind" {
                    base.replace("127.0.0.1", "localhost")
                } else {
                    base
                };
                // The document claims the handle back, in any case, except for `liar.test`.
                let label = did.trim_start_matches("did:plc:");
                let aka = if label == "liar" {
                    "at://someone.else".to_owned()
                } else {
                    format!("at://{}.test", label.to_uppercase())
                };
                Json(json!({
                    "id": did,
                    "alsoKnownAs": [aka],
                    "service": [{"id": "#atproto_pds", "type": "AtprotoPersonalDataServer",
                        "serviceEndpoint": base}],
                }))
            };
            let session = |Json(b): Json<Value>| async move {
                let did = b["identifier"].as_str().unwrap_or_default().to_owned();
                let handle = format!("{}.test", did.trim_start_matches("did:plc:"));
                let scope = match b["password"].as_str() {
                    Some(APP_PASSWORD | NEW_APP_PASSWORD) => "com.atproto.appPass",
                    Some(DM_APP_PASSWORD) => "com.atproto.appPassPrivileged",
                    Some(ACCOUNT_PASSWORD) => "com.atproto.access",
                    Some("needs-2fa") => {
                        return (
                            axum::http::StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "AuthFactorTokenRequired"})),
                        )
                    }
                    _ => {
                        return (
                            axum::http::StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "AuthenticationRequired"})),
                        )
                    }
                };
                let body = json!({"did": did, "handle": handle, "accessJwt": jwt_for(scope, &did),
                    "refreshJwt": "r"});
                (axum::http::StatusCode::OK, Json(body))
            };
            let profile = |Query(q): Query<HashMap<String, String>>| async move {
                let did = q.get("actor").cloned().unwrap_or_default();
                Json(json!({"did": did, "displayName": format!("Name of {did}"),
                    "avatar": "https://cdn.example.com/a.jpg"}))
            };
            let app = Router::new()
                .route("/xrpc/com.atproto.identity.resolveHandle", get(resolve))
                .route("/xrpc/com.atproto.server.createSession", post(session))
                .route("/xrpc/app.bsky.actor.getProfile", get(profile))
                .route("/xrpc/com.atproto.repo.uploadBlob", post(upload_blob))
                .route("/xrpc/com.atproto.repo.createRecord", post(create_record))
                .route("/xrpc/com.atproto.repo.deleteRecord", post(delete_record))
                .route(
                    "/xrpc/app.bsky.notification.listNotifications",
                    get(list_notifications),
                )
                .route("/xrpc/chat.bsky.convo.listConvos", get(list_convos))
                .route("/xrpc/chat.bsky.convo.getMessages", get(get_messages))
                .route(
                    "/xrpc/chat.bsky.convo.getConvoAvailability",
                    get(convo_availability),
                )
                .route(
                    "/xrpc/chat.bsky.convo.getConvoForMembers",
                    get(convo_for_members),
                )
                .route("/xrpc/chat.bsky.convo.sendMessage", post(send_message))
                .route("/img.png", get(|| async { image("image/png", 64) }))
                .route("/big.png", get(|| async { image("image/png", 1_000_001) }))
                .route("/page.html", get(|| async { image("text/html", 64) }))
                .route("/{did}", get(doc))
                .with_state(base.clone());
            tokio::spawn(async move { axum::serve(listener, app).await });
            base
        }

        /// What the mock's inbox endpoints answer, per account DID: `notifications` (a list),
        /// `convos` (a list) and `messages` (convo id to list).
        static INBOX: std::sync::Mutex<Option<HashMap<String, Value>>> =
            std::sync::Mutex::new(None);
        /// Every `sendMessage` body the mock took, newest last.
        static SENT: std::sync::Mutex<Vec<Value>> = std::sync::Mutex::new(Vec::new());

        fn set_inbox(did: &str, data: Value) {
            INBOX
                .lock()
                .unwrap()
                .get_or_insert_with(HashMap::new)
                .insert(did.to_owned(), data);
        }

        /// The account a request's token names, and whether the token has DM access.
        fn caller(headers: &axum::http::HeaderMap) -> Option<(String, bool)> {
            let jwt = headers.get("authorization")?.to_str().ok()?;
            let part = jwt.strip_prefix("Bearer ")?.split('.').nth(1)?;
            let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).ok()?).ok()?;
            let privileged = claims["scope"] == "com.atproto.appPassPrivileged";
            Some((claims["sub"].as_str()?.to_owned(), privileged))
        }

        fn inbox_of(did: &str) -> Value {
            INBOX
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|m| m.get(did).cloned())
                .unwrap_or_else(|| json!({}))
        }

        type Answer = (axum::http::StatusCode, Json<Value>);

        fn refused(status: u16, error: &str) -> Answer {
            (
                axum::http::StatusCode::from_u16(status).unwrap(),
                Json(json!({ "error": error })),
            )
        }

        async fn list_notifications(headers: axum::http::HeaderMap) -> Answer {
            let Some((did, _)) = caller(&headers) else {
                return refused(401, "AuthenticationRequired");
            };
            let list = inbox_of(&did)["notifications"].clone();
            let list = if list.is_null() { json!([]) } else { list };
            (
                axum::http::StatusCode::OK,
                Json(json!({ "notifications": list })),
            )
        }

        /// The chat proxy: refuses tokens without DM access, like Bluesky.
        fn chat_caller(headers: &axum::http::HeaderMap) -> Result<String, Answer> {
            let proxied = headers
                .get("atproto-proxy")
                .is_some_and(|v| v == "did:web:api.bsky.chat#bsky_chat");
            match caller(headers) {
                Some((did, true)) if proxied => Ok(did),
                Some(_) => Err(refused(400, "InvalidToken")),
                None => Err(refused(401, "AuthenticationRequired")),
            }
        }

        async fn list_convos(headers: axum::http::HeaderMap) -> Answer {
            let did = match chat_caller(&headers) {
                Ok(d) => d,
                Err(a) => return a,
            };
            let list = inbox_of(&did)["convos"].clone();
            let list = if list.is_null() { json!([]) } else { list };
            (axum::http::StatusCode::OK, Json(json!({ "convos": list })))
        }

        async fn get_messages(
            headers: axum::http::HeaderMap,
            Query(q): Query<HashMap<String, String>>,
        ) -> Answer {
            let did = match chat_caller(&headers) {
                Ok(d) => d,
                Err(a) => return a,
            };
            let convo = q.get("convoId").cloned().unwrap_or_default();
            let list = inbox_of(&did)["messages"][&convo].clone();
            (
                axum::http::StatusCode::OK,
                Json(json!({ "messages": list })),
            )
        }

        /// Members whose DID starts with `did:plc:closed` take no DMs.
        async fn convo_availability(
            headers: axum::http::HeaderMap,
            Query(q): Query<HashMap<String, String>>,
        ) -> Answer {
            if let Err(a) = chat_caller(&headers) {
                return a;
            }
            let to = q.get("members").cloned().unwrap_or_default();
            let can = !to.starts_with("did:plc:closed");
            (axum::http::StatusCode::OK, Json(json!({ "canChat": can })))
        }

        async fn convo_for_members(
            headers: axum::http::HeaderMap,
            Query(q): Query<HashMap<String, String>>,
        ) -> Answer {
            if let Err(a) = chat_caller(&headers) {
                return a;
            }
            let to = q.get("members").cloned().unwrap_or_default();
            let convo = json!({ "id": format!("convo-{to}") });
            (axum::http::StatusCode::OK, Json(json!({ "convo": convo })))
        }

        /// Text with `BLOCKED` is refused the way the chat service refuses a closed inbox.
        async fn send_message(headers: axum::http::HeaderMap, Json(b): Json<Value>) -> Answer {
            if let Err(a) = chat_caller(&headers) {
                return a;
            }
            if b["message"]["text"]
                .as_str()
                .unwrap_or_default()
                .contains("BLOCKED")
            {
                return refused(400, "InvalidRequest");
            }
            SENT.lock().unwrap().push(b);
            let id = format!("m{}", &uuid::Uuid::now_v7().simple().to_string()[20..]);
            (axum::http::StatusCode::OK, Json(json!({ "id": id })))
        }

        /// Every `createRecord` body the mock took, newest last.
        static RECORDS: std::sync::Mutex<Vec<Value>> = std::sync::Mutex::new(Vec::new());

        fn image(content_type: &'static str, len: usize) -> axum::response::Response {
            use axum::response::IntoResponse;
            ([("content-type", content_type)], vec![7u8; len]).into_response()
        }

        fn authorized(headers: &axum::http::HeaderMap) -> bool {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("Bearer e30."))
        }

        async fn upload_blob(
            headers: axum::http::HeaderMap,
            bytes: axum::body::Bytes,
        ) -> (axum::http::StatusCode, Json<Value>) {
            if !authorized(&headers) {
                return (axum::http::StatusCode::UNAUTHORIZED, Json(json!({})));
            }
            let mime = headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let blob = json!({"$type": "blob", "ref": {"$link": "bafkmock"},
                "mimeType": mime, "size": bytes.len()});
            (axum::http::StatusCode::OK, Json(json!({ "blob": blob })))
        }

        /// Text with `REJECT` is refused with a leaky message; text with `STICKY` gets a
        /// record key that cannot be deleted.
        async fn create_record(
            headers: axum::http::HeaderMap,
            Json(b): Json<Value>,
        ) -> (axum::http::StatusCode, Json<Value>) {
            if !authorized(&headers) {
                return (axum::http::StatusCode::UNAUTHORIZED, Json(json!({})));
            }
            let text = b["record"]["text"].as_str().unwrap_or_default().to_owned();
            if text.contains("REJECT") {
                let leak = json!({"error": "InvalidRequest",
                    "message": "see https://secret.example/?token=abc"});
                return (axum::http::StatusCode::BAD_REQUEST, Json(leak));
            }
            RECORDS.lock().unwrap().push(b.clone());
            let prefix = if text.contains("STICKY") {
                "sticky"
            } else {
                "r"
            };
            let rkey = format!(
                "{prefix}{}",
                &uuid::Uuid::now_v7().simple().to_string()[20..]
            );
            let uri = format!(
                "at://{}/app.bsky.feed.post/{rkey}",
                b["repo"].as_str().unwrap()
            );
            (
                axum::http::StatusCode::OK,
                Json(json!({"uri": uri, "cid": "bafycid"})),
            )
        }

        async fn delete_record(
            headers: axum::http::HeaderMap,
            Json(b): Json<Value>,
        ) -> (axum::http::StatusCode, Json<Value>) {
            if !authorized(&headers) {
                return (axum::http::StatusCode::UNAUTHORIZED, Json(json!({})));
            }
            if b["rkey"].as_str().unwrap_or_default().starts_with("sticky") {
                let err = json!({"error": "InternalServerError"});
                return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, Json(err));
            }
            (axum::http::StatusCode::OK, Json(json!({})))
        }

        /// A brain whose Bluesky calls go to the mock; `secure` keeps the https-only PDS rule.
        async fn server_on_mock(secure: bool) -> (TestServer, DatabaseConnection) {
            let base = mock_bluesky().await;
            server_where(|c| {
                c.bluesky.appview = base.clone();
                c.bluesky.plc = base;
                c.bluesky.allow_insecure_pds = !secure;
            })
            .await
        }

        fn new_handle() -> String {
            format!("u{}.test", &uuid::Uuid::now_v7().simple().to_string()[20..])
        }

        async fn connect(
            s: &TestServer,
            cookie: &str,
            handle: &str,
            pw: &str,
        ) -> axum_test::TestResponse {
            req(s, Method::POST, "/v1/connections/bluesky", cookie)
                .json(&json!({"handle": handle, "app_password": pw}))
                .await
        }

        async fn default_set(db: &DatabaseConnection, team_id: &str) -> uuid::Uuid {
            social_sets::Entity::find()
                .filter(social_sets::Column::TeamId.eq(team_id.parse::<uuid::Uuid>().unwrap()))
                .filter(social_sets::Column::IsDefault.eq(true))
                .one(db)
                .await
                .unwrap()
                .unwrap()
                .id
        }

        async fn list(s: &TestServer, cookie: &str) -> Vec<Value> {
            let res = s.get("/v1/connections").add_header("cookie", cookie).await;
            res.assert_status_ok();
            res.json()
        }

        fn cipher() -> socialreins_core::crypto::Cipher {
            let key = std::env::var("ENCRYPTION_KEY").unwrap();
            socialreins_core::crypto::Cipher::from_base64(&key).unwrap()
        }

        /// An owner with one connected account: (cookie, team id, account id).
        async fn owner_with_account(
            s: &TestServer,
            db: &DatabaseConnection,
        ) -> (String, String, String) {
            let (_, cookie, _) = user(s, db).await;
            let team_id = current(s, &cookie).await["id"].as_str().unwrap().to_owned();
            let res = connect(s, &cookie, &new_handle(), APP_PASSWORD).await;
            assert_eq!(res.status_code(), 201);
            let id = res.json::<Value>()["id"].as_str().unwrap().to_owned();
            (cookie, team_id, id)
        }

        #[tokio::test]
        #[serial]
        async fn connect_joins_the_default_set_and_reconnect_updates() {
            let (s, db) = server_on_mock(false).await;
            let (_, cookie, user_id) = user(&s, &db).await;
            let team_id = current(&s, &cookie).await["id"]
                .as_str()
                .unwrap()
                .to_owned();
            let handle = new_handle();
            let label = handle.trim_end_matches(".test");
            let res = connect(
                &s,
                &cookie,
                &format!(" @{}", handle.to_uppercase()),
                APP_PASSWORD,
            )
            .await;
            assert_eq!(res.status_code(), 201);
            let body = res.json::<Value>();
            let set = default_set(&db, &team_id).await;
            assert_eq!(body["provider"], "bluesky");
            assert_eq!(body["external_id"], format!("did:plc:{label}"));
            assert_eq!(body["handle"], handle);
            assert_eq!(body["display_name"], format!("Name of did:plc:{label}"));
            assert_eq!(body["avatar_url"], "https://cdn.example.com/a.jpg");
            assert_eq!(body["set_ids"], json!([set.to_string()]));
            assert!(body.get("access_token").is_none() && body.get("meta").is_none());

            let id: uuid::Uuid = body["id"].as_str().unwrap().parse().unwrap();
            let row = social_accounts::Entity::find_by_id(id)
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row.created_by, Some(user_id));
            assert!(!row.access_token.windows(4).any(|w| w == b"abcd"));
            assert_eq!(
                cipher().decrypt(&row.access_token).unwrap(),
                APP_PASSWORD.as_bytes()
            );
            assert!(row.meta["pds"]
                .as_str()
                .unwrap()
                .starts_with("http://127.0.0.1:"));

            let again = connect(&s, &cookie, &handle, NEW_APP_PASSWORD).await;
            assert_eq!(again.status_code(), 200);
            assert_eq!(again.json::<Value>()["id"], body["id"]);
            let all = list(&s, &cookie).await;
            assert_eq!(all.len(), 1);
            assert_eq!(all[0]["set_ids"], json!([set.to_string()]));
            assert!(all[0].get("access_token").is_none());
            let row = social_accounts::Entity::find_by_id(id)
                .one(&db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                cipher().decrypt(&row.access_token).unwrap(),
                NEW_APP_PASSWORD.as_bytes()
            );
        }

        #[tokio::test]
        #[serial]
        async fn wrong_passwords_and_handles_are_refused() {
            let (s, db) = server_on_mock(false).await;
            let (_, cookie, _) = user(&s, &db).await;
            let cases = [
                (new_handle(), "wrong-pass", 422, "invalid_credentials"),
                (new_handle(), "", 422, "invalid_credentials"),
                (new_handle(), ACCOUNT_PASSWORD, 422, "invalid_credentials"),
                (new_handle(), "needs-2fa", 422, "invalid_credentials"),
                ("nobody.test".into(), APP_PASSWORD, 422, "invalid_handle"),
                ("not a handle".into(), APP_PASSWORD, 422, "invalid_handle"),
            ];
            for (i, (handle, pw, status, code)) in cases.into_iter().enumerate() {
                // Past 5 attempts the per-user limit would answer instead.
                let cookie = if i < 5 {
                    cookie.clone()
                } else {
                    user(&s, &db).await.1
                };
                let res = connect(&s, &cookie, &handle, pw).await;
                assert_eq!(res.status_code(), status, "{code}");
                res.assert_json(&json!({"error": code}));
            }
            assert!(list(&s, &cookie).await.is_empty());
        }

        #[tokio::test]
        #[serial]
        async fn a_pds_must_be_https() {
            let (s, db) = server_on_mock(true).await;
            let (_, cookie, _) = user(&s, &db).await;
            let res = connect(&s, &cookie, &new_handle(), APP_PASSWORD).await;
            assert_eq!(res.status_code(), 422);
            res.assert_json(&json!({"error": "unsupported_pds"}));
        }

        #[tokio::test]
        #[serial]
        async fn connect_attempts_are_throttled_per_user() {
            let (s, db) = server_on_mock(false).await;
            let (_, cookie, _) = user(&s, &db).await;
            for _ in 0..5 {
                let res = connect(&s, &cookie, &new_handle(), "wrong").await;
                assert_eq!(res.status_code(), 422);
            }
            let res = connect(&s, &cookie, &new_handle(), APP_PASSWORD).await;
            assert_eq!(res.status_code(), 429);
            res.assert_json(&json!({"error": "rate_limited"}));
        }

        fn did_of(handle: &str) -> String {
            format!("did:plc:{}", handle.trim_end_matches(".test"))
        }

        async fn hits(db: &DatabaseConnection, bucket: &str) -> i64 {
            use sea_orm::{ConnectionTrait, DbBackend, Statement};
            let sql = "SELECT count(*) AS n FROM rate_limit_hits WHERE bucket = $1";
            let stmt = Statement::from_sql_and_values(DbBackend::Postgres, sql, [bucket.into()]);
            let row = db.query_one_raw(stmt).await.unwrap().unwrap();
            row.try_get("", "n").unwrap()
        }

        #[tokio::test]
        #[serial]
        async fn the_daily_account_cap_is_per_team_and_outages_are_refunded() {
            use sea_orm::{ConnectionTrait, DbBackend, Statement};
            let (s, db) = server_on_mock(false).await;
            let (_, cookie, user_id) = user(&s, &db).await;
            let team_id = current(&s, &cookie).await["id"]
                .as_str()
                .unwrap()
                .to_owned();
            let handle = new_handle();
            let bucket = format!("bluesky:did:{team_id}:{}", did_of(&handle));
            for _ in 0..10 {
                let sql = "INSERT INTO rate_limit_hits (bucket) VALUES ($1)";
                let stmt = Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    sql,
                    [bucket.clone().into()],
                );
                db.execute_raw(stmt).await.unwrap();
            }
            let res = connect(&s, &cookie, &handle, APP_PASSWORD).await;
            assert_eq!(res.status_code(), 429);
            let (_, other, _) = user(&s, &db).await;
            let res = connect(&s, &other, &handle, APP_PASSWORD).await;
            assert_eq!(res.status_code(), 201, "another team is not blocked");

            let res = connect(&s, &cookie, "down.test", APP_PASSWORD).await;
            assert_eq!(res.status_code(), 502);
            res.assert_json(&json!({"error": "bluesky_unreachable"}));
            assert_eq!(hits(&db, &format!("bluesky:user:{user_id}")).await, 0);
            // Only refused passwords stay in the cross-team failure count.
            let failed = format!("bluesky:failed:{}", did_of(&handle));
            assert_eq!(hits(&db, &failed).await, 0, "a success is not a failure");
            let wrong = new_handle();
            let (_, third, _) = user(&s, &db).await;
            let res = connect(&s, &third, &wrong, "wrong-pass").await;
            assert_eq!(res.status_code(), 422);
            assert_eq!(
                hits(&db, &format!("bluesky:failed:{}", did_of(&wrong))).await,
                1
            );
        }

        #[tokio::test]
        #[serial]
        async fn handles_that_do_not_resolve_count_against_the_user_cap() {
            use sea_orm::{ConnectionTrait, DbBackend, Statement};
            let (s, db) = server_on_mock(false).await;
            let (_, cookie, user_id) = user(&s, &db).await;
            let bucket = format!("bluesky:user:{user_id}");
            // 29 of the 30 a day used; the burst limit allows only 5 a minute.
            for _ in 0..29 {
                let sql = "INSERT INTO rate_limit_hits (bucket) VALUES ($1)";
                let stmt = Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    sql,
                    [bucket.clone().into()],
                );
                db.execute_raw(stmt).await.unwrap();
            }
            let res = connect(&s, &cookie, "nobody.test", APP_PASSWORD).await;
            assert_eq!(res.status_code(), 422);
            res.assert_json(&json!({"error": "invalid_handle"}));
            assert_eq!(hits(&db, &bucket).await, 30);
            let res = connect(&s, &cookie, "nobody.test", APP_PASSWORD).await;
            assert_eq!(res.status_code(), 429);
            res.assert_json(&json!({"error": "rate_limited"}));
            assert_eq!(hits(&db, &bucket).await, 30);
        }

        #[tokio::test]
        #[serial]
        async fn failures_on_one_account_are_capped_across_teams() {
            use sea_orm::{ConnectionTrait, DbBackend, Statement};
            let (s, db) = server_on_mock(false).await;
            let handle = new_handle();
            let (_, holder, _) = user(&s, &db).await;
            assert_eq!(
                connect(&s, &holder, &handle, APP_PASSWORD)
                    .await
                    .status_code(),
                201
            );
            let bucket = format!("bluesky:failed:{}", did_of(&handle));
            for _ in 0..20 {
                let sql = "INSERT INTO rate_limit_hits (bucket) VALUES ($1)";
                let stmt = Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    sql,
                    [bucket.clone().into()],
                );
                db.execute_raw(stmt).await.unwrap();
            }
            let (_, cookie, user_id) = user(&s, &db).await;
            let res = connect(&s, &cookie, &handle, APP_PASSWORD).await;
            assert_eq!(res.status_code(), 429);
            res.assert_json(&json!({"error": "rate_limited"}));
            assert_eq!(hits(&db, &bucket).await, 20);
            // The team that holds the account still replaces its app password.
            let res = connect(&s, &holder, &handle, NEW_APP_PASSWORD).await;
            assert_eq!(res.status_code(), 200);
            assert_eq!(
                hits(&db, &format!("bluesky:user:{user_id}")).await,
                0,
                "refunded"
            );
        }

        #[tokio::test]
        #[serial]
        async fn private_addresses_and_huge_bodies_are_refused() {
            let (s, db) = server_on_mock(false).await;
            let (_, cookie, _) = user(&s, &db).await;
            let res = connect(&s, &cookie, "liar.test", APP_PASSWORD).await;
            assert_eq!(res.status_code(), 422, "the DID must claim the handle back");
            res.assert_json(&json!({"error": "invalid_handle"}));
            let res = connect(&s, &cookie, "rebind.test", APP_PASSWORD).await;
            assert_eq!(res.status_code(), 422);
            res.assert_json(&json!({"error": "unsupported_pds"}));
            let res = connect(&s, &cookie, "big.test", APP_PASSWORD).await;
            assert_eq!(res.status_code(), 502);
            res.assert_json(&json!({"error": "bluesky_unreachable"}));
            assert!(list(&s, &cookie).await.is_empty());
        }

        #[tokio::test]
        #[serial]
        async fn members_list_but_cannot_manage() {
            let (s, db) = server_on_mock(false).await;
            let (owner, team_id, id) = owner_with_account(&s, &db).await;
            let (member, _) = joined(&s, &db, &team_id, "member").await;
            let set = default_set(&db, &team_id).await;
            let calls = [
                connect(&s, &member, &new_handle(), APP_PASSWORD).await,
                req(
                    &s,
                    Method::PUT,
                    &format!("/v1/connections/{id}/sets"),
                    &member,
                )
                .json(&json!({"set_ids": [set]}))
                .await,
                req(
                    &s,
                    Method::DELETE,
                    &format!("/v1/connections/{id}"),
                    &member,
                )
                .await,
            ];
            for res in calls {
                assert_eq!(res.status_code(), 403);
                res.assert_json(&json!({"error": "forbidden"}));
            }
            assert_eq!(list(&s, &member).await.len(), 1);
            let (admin, _) = joined(&s, &db, &team_id, "admin").await;
            // Disconnecting needs a fresh sign-in.
            let row = sessions::ActiveModel {
                token_hash: Set(token::hash(owner.trim_start_matches("sr_session="))),
                created_at: Set((Utc::now() - Duration::minutes(11)).fixed_offset()),
                ..Default::default()
            };
            sessions::Entity::update(row).exec(&db).await.unwrap();
            let res = req(&s, Method::DELETE, &format!("/v1/connections/{id}"), &owner).await;
            assert_eq!(res.status_code(), 403);
            res.assert_json(&json!({"error": "reauth_required"}));
            let res = req(&s, Method::DELETE, &format!("/v1/connections/{id}"), &admin).await;
            assert_eq!(res.status_code(), 204);
            assert!(list(&s, &owner).await.is_empty());
        }

        #[tokio::test]
        #[serial]
        async fn another_teams_account_is_not_found() {
            let (s, db) = server_on_mock(false).await;
            let (_, team_id, id) = owner_with_account(&s, &db).await;
            let (_, stranger, _) = user(&s, &db).await;
            let set = default_set(&db, &team_id).await;
            let res = req(
                &s,
                Method::PUT,
                &format!("/v1/connections/{id}/sets"),
                &stranger,
            )
            .json(&json!({"set_ids": [set]}))
            .await;
            assert_eq!(res.status_code(), 404);
            let res = req(
                &s,
                Method::DELETE,
                &format!("/v1/connections/{id}"),
                &stranger,
            )
            .await;
            assert_eq!(res.status_code(), 404);
            res.assert_json(&json!({"error": "not_found"}));
            assert!(list(&s, &stranger).await.is_empty());
            let still = social_accounts::Entity::find_by_id(id.parse::<uuid::Uuid>().unwrap())
                .one(&db)
                .await
                .unwrap();
            assert!(still.is_some());
        }

        #[tokio::test]
        #[serial]
        async fn sets_must_be_the_teams_and_keep_the_default() {
            let (s, db) = server_on_mock(false).await;
            let (cookie, team_id, id) = owner_with_account(&s, &db).await;
            let default = default_set(&db, &team_id).await;
            let new_set = |team: uuid::Uuid| social_sets::ActiveModel {
                id: Set(uuid::Uuid::now_v7()),
                team_id: Set(team),
                name: Set("Clients".into()),
                is_default: Set(false),
                ..Default::default()
            };
            let other = social_sets::Entity::insert(new_set(team_id.parse().unwrap()))
                .exec_with_returning(&db)
                .await
                .unwrap()
                .id;
            let (_, stranger, _) = user(&s, &db).await;
            let foreign_team = current(&s, &stranger).await["id"]
                .as_str()
                .unwrap()
                .to_owned();
            let foreign = default_set(&db, &foreign_team).await;
            let put = |sets: Value| {
                req(
                    &s,
                    Method::PUT,
                    &format!("/v1/connections/{id}/sets"),
                    &cookie,
                )
                .json(&json!({ "set_ids": sets }))
            };
            let res = put(json!([other, default, other])).await;
            res.assert_status_ok();
            let mut want = vec![default.to_string(), other.to_string()];
            want.sort();
            assert_eq!(res.json::<Value>()["set_ids"], json!(want));
            for (sets, code) in [
                (json!([other]), "default_set_required"),
                (json!([]), "default_set_required"),
                (json!([default, foreign]), "invalid_sets"),
                (json!([default, uuid::Uuid::now_v7()]), "invalid_sets"),
            ] {
                let res = put(sets).await;
                assert_eq!(res.status_code(), 422, "{code}");
                res.assert_json(&json!({"error": code}));
            }
            assert_eq!(list(&s, &cookie).await[0]["set_ids"], json!(want));
            let res = put(json!([default])).await;
            res.assert_status_ok();
            assert_eq!(res.json::<Value>()["set_ids"], json!([default.to_string()]));
        }

        #[tokio::test]
        #[serial]
        async fn delete_removes_the_account_and_its_sets() {
            let (s, db) = server_on_mock(false).await;
            let (cookie, _, id) = owner_with_account(&s, &db).await;
            let path = format!("/v1/connections/{id}");
            assert_eq!(
                req(&s, Method::DELETE, &path, &cookie).await.status_code(),
                204
            );
            assert!(list(&s, &cookie).await.is_empty());
            let links = social_account_social_set::Entity::find()
                .filter(
                    social_account_social_set::Column::SocialAccountId
                        .eq(id.parse::<uuid::Uuid>().unwrap()),
                )
                .count(&db)
                .await
                .unwrap();
            assert_eq!(links, 0);
            assert_eq!(
                req(&s, Method::DELETE, &path, &cookie).await.status_code(),
                404
            );
        }

        #[tokio::test]
        #[serial]
        async fn providers_and_routes_need_a_session() {
            let (s, db) = server_on_mock(false).await;
            for path in ["/v1/connections", "/v1/connections/providers"] {
                assert_eq!(s.get(path).await.status_code(), 401, "{path}");
            }
            let (_, cookie, _) = user(&s, &db).await;
            let res = s
                .get("/v1/connections/providers")
                .add_header("cookie", &cookie)
                .await;
            res.assert_status_ok();
            let body = res.json::<Vec<Value>>();
            let names: Vec<&str> = body
                .iter()
                .map(|p| p["provider"].as_str().unwrap())
                .collect();
            assert_eq!(
                names,
                ["bluesky", "meta", "threads", "linkedin", "x", "tiktok", "youtube"]
            );
            assert_eq!(body[0]["configured"], true);
            // The web origin check on a state-changing route.
            let res = s
                .post("/v1/connections/bluesky")
                .add_header("cookie", &cookie)
                .json(&json!({"handle": new_handle(), "app_password": APP_PASSWORD}))
                .await;
            assert_eq!(res.status_code(), 403);
            res.assert_json(&json!({"error": "forbidden_origin"}));
            let doc = s.get("/openapi.json").await.json::<Value>();
            for (path, method) in [
                ("/connections", "get"),
                ("/connections/providers", "get"),
                ("/connections/bluesky", "post"),
                ("/connections/{id}/sets", "put"),
                ("/connections/{id}", "delete"),
            ] {
                assert!(doc["paths"][path][method].is_object(), "{path} {method}");
            }
        }

        /// `/v1/social-sets`.
        mod sets {
            use super::*;
            use socialreins_brain::models::sessions;
            use socialreins_brain::scheduler;

            async fn make(s: &TestServer, cookie: &str, name: &str) -> axum_test::TestResponse {
                req(s, Method::POST, "/v1/social-sets", cookie)
                    .json(&json!({ "name": name }))
                    .await
            }

            async fn sets(s: &TestServer, cookie: &str, query: &str) -> Vec<Value> {
                let res = s
                    .get(&format!("/v1/social-sets{query}"))
                    .add_header("cookie", cookie)
                    .await;
                res.assert_status_ok();
                res.json()
            }

            fn id_of(res: &axum_test::TestResponse) -> String {
                res.json::<Value>()["id"].as_str().unwrap().to_owned()
            }

            /// Makes the session older than the 10-minute fresh sign-in window.
            async fn stale(db: &DatabaseConnection, cookie: &str) {
                let row = sessions::ActiveModel {
                    token_hash: Set(token::hash(cookie.trim_start_matches("sr_session="))),
                    created_at: Set((Utc::now() - Duration::minutes(11)).fixed_offset()),
                    ..Default::default()
                };
                sessions::Entity::update(row).exec(db).await.unwrap();
            }

            async fn deleted_days_ago(db: &DatabaseConnection, id: &str, days: i64) {
                let row = social_sets::ActiveModel {
                    id: Set(id.parse().unwrap()),
                    deleted_at: Set(Some((Utc::now() - Duration::days(days)).fixed_offset())),
                    ..Default::default()
                };
                social_sets::Entity::update(row).exec(db).await.unwrap();
            }

            #[tokio::test]
            #[serial]
            async fn create_list_and_rename() {
                let (s, db) = server_where(|_| {}).await;
                let (_, cookie, _) = user(&s, &db).await;
                let res = make(&s, &cookie, "  Clients ").await;
                assert_eq!(res.status_code(), 201);
                let body = res.json::<Value>();
                assert_eq!(body["name"], "Clients");
                assert_eq!(body["is_default"], false);
                assert_eq!(body["account_count"], 0);
                let all = sets(&s, &cookie, "").await;
                let names: Vec<&str> = all.iter().map(|s| s["name"].as_str().unwrap()).collect();
                assert_eq!(names, ["Default", "Clients"], "Default first");

                for (name, status, code) in [
                    ("CLIENTS", 409, "name_taken"),
                    ("default", 409, "name_taken"),
                    ("   ", 422, "invalid_name"),
                    (&"x".repeat(61)[..], 422, "invalid_name"),
                ] {
                    let res = make(&s, &cookie, name).await;
                    assert_eq!(res.status_code(), status, "{name}");
                    res.assert_json(&json!({ "error": code }));
                }

                let id = body["id"].as_str().unwrap();
                let rename = |id: String, name: &'static str| {
                    req(&s, Method::PATCH, &format!("/v1/social-sets/{id}"), &cookie)
                        .json(&json!({ "name": name }))
                };
                let res = rename(id.into(), "Agency").await;
                res.assert_status_ok();
                assert_eq!(res.json::<Value>()["name"], "Agency");
                assert_eq!(rename(id.into(), "DEFAULT").await.status_code(), 409);
                // The Default set is pinned, not named: it can be renamed.
                let default = all[0]["id"].as_str().unwrap().to_owned();
                let res = rename(default, "Everything").await;
                res.assert_status_ok();
                assert_eq!(res.json::<Value>()["is_default"], true);
                let res = rename(uuid::Uuid::now_v7().to_string(), "Nope").await;
                assert_eq!(res.status_code(), 404);
                // A deleted set cannot be renamed.
                req(
                    &s,
                    Method::DELETE,
                    &format!("/v1/social-sets/{id}"),
                    &cookie,
                )
                .json(&json!({}))
                .await
                .assert_status(axum::http::StatusCode::NO_CONTENT);
                let res = rename(id.into(), "Ghost").await;
                assert_eq!(res.status_code(), 404);
                res.assert_json(&json!({"error": "not_found"}));
                let res = s
                    .get("/v1/social-sets?deleted=maybe")
                    .add_header("cookie", &cookie)
                    .await;
                assert_eq!(res.status_code(), 422);
                res.assert_json(&json!({"error": "invalid_query"}));
            }

            #[tokio::test]
            #[serial]
            async fn a_team_holds_at_most_50_live_sets() {
                let (s, db) = server_where(|_| {}).await;
                let (_, cookie, _) = user(&s, &db).await;
                let mut last = String::new();
                for i in 1..50 {
                    let res = make(&s, &cookie, &format!("Set {i}")).await;
                    assert_eq!(res.status_code(), 201);
                    last = id_of(&res);
                }
                let res = make(&s, &cookie, "One too many").await;
                assert_eq!(res.status_code(), 409);
                res.assert_json(&json!({"error": "too_many_sets"}));
                let path = format!("/v1/social-sets/{last}");
                let res = req(&s, Method::DELETE, &path, &cookie)
                    .json(&json!({}))
                    .await;
                assert_eq!(res.status_code(), 204);
                assert_eq!(make(&s, &cookie, "Room again").await.status_code(), 201);
                let res = req(&s, Method::POST, &format!("{path}/restore"), &cookie).await;
                assert_eq!(res.status_code(), 409);
                res.assert_json(&json!({"error": "too_many_sets"}));
            }

            #[tokio::test]
            #[serial]
            async fn delete_needs_a_fresh_sign_in_and_spares_the_default() {
                let (s, db) = server_where(|_| {}).await;
                let (_, cookie, _) = user(&s, &db).await;
                let default = sets(&s, &cookie, "").await[0]["id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let res = req(
                    &s,
                    Method::DELETE,
                    &format!("/v1/social-sets/{default}"),
                    &cookie,
                )
                .json(&json!({}))
                .await;
                assert_eq!(res.status_code(), 409);
                res.assert_json(&json!({"error": "default_set"}));

                let id = id_of(&make(&s, &cookie, "Old").await);
                let path = format!("/v1/social-sets/{id}");
                stale(&db, &cookie).await;
                let res = req(&s, Method::DELETE, &path, &cookie)
                    .json(&json!({}))
                    .await;
                assert_eq!(res.status_code(), 403);
                res.assert_json(&json!({"error": "reauth_required"}));
                assert_eq!(sets(&s, &cookie, "").await.len(), 2, "still there");
            }

            #[tokio::test]
            #[serial]
            async fn deleted_sets_keep_accounts_for_restore_then_get_purged() {
                let (s, db) = server_on_mock(false).await;
                let (cookie, team_id, account) = owner_with_account(&s, &db).await;
                let default = default_set(&db, &team_id).await.to_string();
                let id = id_of(&make(&s, &cookie, "Clients").await);
                let put = |ids: Value| {
                    req(
                        &s,
                        Method::PUT,
                        &format!("/v1/connections/{account}/sets"),
                        &cookie,
                    )
                    .json(&json!({ "set_ids": ids }))
                };
                put(json!([default, id])).await.assert_status_ok();
                let path = format!("/v1/social-sets/{id}");
                let res = req(&s, Method::DELETE, &path, &cookie)
                    .json(&json!({}))
                    .await;
                assert_eq!(res.status_code(), 204);
                assert_eq!(sets(&s, &cookie, "").await.len(), 1);
                let trash = sets(&s, &cookie, "?deleted=true").await;
                assert_eq!(trash.len(), 1);
                assert_eq!(trash[0]["account_count"], 1);
                assert!(trash[0]["deleted_at"].is_string());
                assert_eq!(list(&s, &cookie).await[0]["set_ids"], json!([default]));
                let res = put(json!([default, id])).await;
                assert_eq!(res.status_code(), 422, "a deleted set cannot be chosen");
                res.assert_json(&json!({"error": "invalid_sets"}));
                put(json!([default])).await.assert_status_ok();
                // A name freed by a delete can be taken; restoring then collides.
                let twin = id_of(&make(&s, &cookie, "clients").await);
                let restore = format!("{path}/restore");
                let res = req(&s, Method::POST, &restore, &cookie).await;
                assert_eq!(res.status_code(), 409);
                res.assert_json(&json!({"error": "name_taken"}));
                let twin_path = format!("/v1/social-sets/{twin}");
                req(&s, Method::DELETE, &twin_path, &cookie)
                    .json(&json!({}))
                    .await;
                let res = req(&s, Method::POST, &restore, &cookie).await;
                res.assert_status_ok();
                let body = res.json::<Value>();
                assert_eq!(
                    body["account_ids"],
                    json!([account]),
                    "links kept while deleted"
                );
                assert!(body["deleted_at"].is_null());
                let mut want = vec![default.clone(), id.clone()];
                want.sort();
                assert_eq!(list(&s, &cookie).await[0]["set_ids"], json!(want));
                let res = req(&s, Method::POST, &restore, &cookie).await;
                assert_eq!(res.status_code(), 404, "a live set has nothing to restore");

                // Past 30 days: no restore, and the purge removes it with its links.
                req(&s, Method::DELETE, &path, &cookie)
                    .json(&json!({}))
                    .await;
                deleted_days_ago(&db, &id, 31).await;
                deleted_days_ago(&db, &twin, 29).await;
                assert_eq!(
                    req(&s, Method::POST, &restore, &cookie).await.status_code(),
                    404
                );
                assert!(scheduler::purge_deleted_sets(&db).await.unwrap() >= 1);
                let gone = social_sets::Entity::find_by_id(id.parse::<uuid::Uuid>().unwrap())
                    .one(&db)
                    .await
                    .unwrap();
                assert!(gone.is_none());
                let links = social_account_social_set::Entity::find()
                    .filter(
                        social_account_social_set::Column::SocialSetId
                            .eq(id.parse::<uuid::Uuid>().unwrap()),
                    )
                    .count(&db)
                    .await
                    .unwrap();
                assert_eq!(links, 0);
                let kept = social_sets::Entity::find_by_id(twin.parse::<uuid::Uuid>().unwrap())
                    .one(&db)
                    .await
                    .unwrap();
                assert!(kept.is_some(), "29 days old stays restorable");
                assert_eq!(
                    scheduler::purge_deleted_sets(&db).await.unwrap(),
                    0,
                    "idempotent"
                );
            }

            #[tokio::test]
            #[serial]
            async fn members_read_and_strangers_get_404() {
                let (s, db) = server_where(|_| {}).await;
                let (_, owner, _) = user(&s, &db).await;
                let team_id = current(&s, &owner).await["id"].as_str().unwrap().to_owned();
                let id = id_of(&make(&s, &owner, "Clients").await);
                let path = format!("/v1/social-sets/{id}");
                let (member, _) = joined(&s, &db, &team_id, "member").await;
                let (_, stranger, _) = user(&s, &db).await;
                for (who, status, code) in
                    [(&member, 403, "forbidden"), (&stranger, 404, "not_found")]
                {
                    let calls = [
                        req(&s, Method::PATCH, &path, who)
                            .json(&json!({"name": "X"}))
                            .await,
                        req(&s, Method::DELETE, &path, who).json(&json!({})).await,
                        req(&s, Method::POST, &format!("{path}/restore"), who).await,
                    ];
                    for res in calls {
                        assert_eq!(res.status_code(), status);
                        res.assert_json(&json!({ "error": code }));
                    }
                }
                let res = make(&s, &member, "Mine").await;
                assert_eq!(res.status_code(), 403);
                assert_eq!(sets(&s, &member, "").await.len(), 2);
                assert_eq!(
                    sets(&s, &stranger, "").await.len(),
                    1,
                    "only their own Default"
                );
                assert_eq!(
                    make(&s, &stranger, "Clients").await.status_code(),
                    201,
                    "names are per team"
                );
                let doc = s.get("/openapi.json").await.json::<Value>();
                for (p, m) in [
                    ("/social-sets", "get"),
                    ("/social-sets", "post"),
                    ("/social-sets/{id}", "patch"),
                    ("/social-sets/{id}", "delete"),
                    ("/social-sets/{id}/restore", "post"),
                ] {
                    assert!(doc["paths"][p][m].is_object(), "{p} {m}");
                }
            }
        }

        /// `/v1/posts` and the publish pipeline, against the mock PDS.
        mod posts_api {
            use super::*;
            use socialreins_brain::{
                models::{post_targets, posts},
                publish,
            };

            /// A brain on the mock, with its state for driving the publish pipeline directly.
            async fn on_mock() -> (TestServer, DatabaseConnection, AppState) {
                let base = mock_bluesky().await;
                let mut config = Config::from_env().expect("test config");
                config.trust_proxy = true;
                config.bluesky.appview = base.clone();
                config.bluesky.plc = base;
                config.bluesky.allow_insecure_pds = true;
                let db = socialreins_brain::connect(&config)
                    .await
                    .expect("database reachable");
                let state = AppState::new(db.clone(), &config);
                let app = controllers::router(state.clone(), &config);
                (TestServer::new(app).unwrap(), db, state)
            }

            fn input(set: &str, body: &str, at: Option<&str>) -> Value {
                json!({"social_set_id": set, "platform": "bluesky", "body": body,
                    "media": [], "scheduled_at": at})
            }

            async fn create(s: &TestServer, cookie: &str, body: Value) -> axum_test::TestResponse {
                req(s, Method::POST, "/v1/posts", cookie).json(&body).await
            }

            async fn get_post(s: &TestServer, cookie: &str, id: &str) -> Value {
                let res = s
                    .get(&format!("/v1/posts/{id}"))
                    .add_header("cookie", cookie)
                    .await;
                res.assert_status_ok();
                res.json()
            }

            /// Polls until the post leaves `queued` (publish now runs in the background).
            async fn settled(s: &TestServer, cookie: &str, id: &str) -> Value {
                for _ in 0..100 {
                    let p = get_post(s, cookie, id).await;
                    if p["status"] != "queued" {
                        return p;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                panic!("post {id} stayed queued");
            }

            fn past() -> String {
                (Utc::now() - Duration::minutes(1)).to_rfc3339()
            }

            /// Points a post's media at the mock (http on loopback, which only tests allow).
            async fn set_media(db: &DatabaseConnection, s_base: &str, id: &str, paths: &[&str]) {
                let urls: Vec<String> = paths.iter().map(|p| format!("{s_base}{p}")).collect();
                let row = posts::ActiveModel {
                    id: Set(id.parse().unwrap()),
                    media: Set(json!(urls)),
                    ..Default::default()
                };
                posts::Entity::update(row).exec(db).await.unwrap();
            }

            async fn pds_of(db: &DatabaseConnection, team_id: &str) -> String {
                let a = social_accounts::Entity::find()
                    .filter(
                        social_accounts::Column::TeamId.eq(team_id.parse::<uuid::Uuid>().unwrap()),
                    )
                    .one(db)
                    .await
                    .unwrap()
                    .unwrap();
                a.meta["pds"].as_str().unwrap().to_owned()
            }

            #[tokio::test]
            #[serial]
            async fn posts_are_validated_listed_and_edited_by_members() {
                let (s, db, _) = on_mock().await;
                let (_, owner, _) = user(&s, &db).await;
                let team_id = current(&s, &owner).await["id"].as_str().unwrap().to_owned();
                let set = default_set(&db, &team_id).await.to_string();
                let (member, _) = joined(&s, &db, &team_id, "member").await;

                let res = create(&s, &member, input(&set, "draft one", None)).await;
                assert_eq!(res.status_code(), 201, "members write posts");
                let draft = res.json::<Value>();
                assert_eq!(draft["status"], "draft");
                let at = (Utc::now() + Duration::days(3)).to_rfc3339();
                let sched = create(&s, &owner, input(&set, "later", Some(&at))).await;
                assert_eq!(sched.json::<Value>()["status"], "scheduled");

                let five: Vec<String> = (0..5)
                    .map(|i| format!("https://a.example/{i}.png"))
                    .collect();
                let mut bad_media = input(&set, "x", None);
                bad_media["media"] = json!(["http://a.example/i.png"]);
                let mut too_many = input(&set, "x", None);
                too_many["media"] = json!(five);
                let mut other_platform = input(&set, "x", None);
                other_platform["platform"] = json!("x");
                for (body, code) in [
                    (input(&set, &"a".repeat(301), None), "too_long"),
                    (input(&set, "  ", None), "empty_post"),
                    (bad_media, "invalid_media"),
                    (too_many, "too_many_images"),
                    (other_platform, "unsupported_platform"),
                    (
                        input(&uuid::Uuid::now_v7().to_string(), "x", None),
                        "invalid_set",
                    ),
                ] {
                    let res = create(&s, &owner, body).await;
                    assert_eq!(res.status_code(), 422, "{code}");
                    res.assert_json(&json!({ "error": code }));
                }

                let list = |q: String| {
                    let s = &s;
                    let owner = owner.clone();
                    async move {
                        let res = s
                            .get(&format!("/v1/posts{q}"))
                            .add_header("cookie", owner)
                            .await;
                        res.assert_status_ok();
                        res.json::<Vec<Value>>()
                    }
                };
                assert_eq!(list(String::new()).await.len(), 2);
                assert_eq!(list("?status=draft".into()).await.len(), 1);
                let from = Utc::now().to_rfc3339().replace('+', "%2B");
                let to = (Utc::now() + Duration::days(7))
                    .to_rfc3339()
                    .replace('+', "%2B");
                let window = list(format!("?from={from}&to={to}&social_set_id={set}")).await;
                assert_eq!(window.len(), 1);
                assert_eq!(window[0]["body"], "later");
                let res = s
                    .get("/v1/posts?status=nope")
                    .add_header("cookie", &owner)
                    .await;
                assert_eq!(res.status_code(), 422);

                let id = draft["id"].as_str().unwrap();
                let path = format!("/v1/posts/{id}");
                let res = req(&s, Method::PUT, &path, &member)
                    .json(&input(&set, "now scheduled", Some(&at)))
                    .await;
                res.assert_status_ok();
                assert_eq!(res.json::<Value>()["status"], "scheduled");
                let res = req(&s, Method::PUT, &path, &member)
                    .json(&input(&set, "back to draft", None))
                    .await;
                assert_eq!(res.json::<Value>()["status"], "draft");

                // Another team sees nothing.
                let (_, stranger, _) = user(&s, &db).await;
                let res = req(&s, Method::PUT, &path, &stranger)
                    .json(&input(&set, "mine", None))
                    .await;
                assert_eq!(res.status_code(), 404);
                assert_eq!(
                    req(&s, Method::DELETE, &path, &stranger)
                        .await
                        .status_code(),
                    404
                );

                // A deleted set hides its posts until it is restored.
                let other = id_of(
                    &req(&s, Method::POST, "/v1/social-sets", &owner)
                        .json(&json!({"name": "Clients"}))
                        .await,
                );
                let p = create(&s, &owner, input(&other, "in clients", None)).await;
                let pid = p.json::<Value>()["id"].as_str().unwrap().to_owned();
                req(
                    &s,
                    Method::DELETE,
                    &format!("/v1/social-sets/{other}"),
                    &owner,
                )
                .json(&json!({}))
                .await;
                assert_eq!(list(String::new()).await.len(), 2);
                let res = s
                    .get(&format!("/v1/posts/{pid}"))
                    .add_header("cookie", &owner)
                    .await;
                assert_eq!(res.status_code(), 404);
                req(
                    &s,
                    Method::POST,
                    &format!("/v1/social-sets/{other}/restore"),
                    &owner,
                )
                .await;
                assert_eq!(list(String::new()).await.len(), 3);

                assert_eq!(
                    req(&s, Method::DELETE, &path, &member).await.status_code(),
                    204
                );
                assert_eq!(list(String::new()).await.len(), 2);
            }

            fn id_of(res: &axum_test::TestResponse) -> String {
                res.json::<Value>()["id"].as_str().unwrap().to_owned()
            }

            #[tokio::test]
            #[serial]
            async fn publish_now_posts_with_link_facets_and_takedown_deletes() {
                let (s, db, _) = on_mock().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let text = "🦀 read https://example.com/a. ok";
                let id = id_of(&create(&s, &cookie, input(&set, text, None)).await);
                let res = req(
                    &s,
                    Method::POST,
                    &format!("/v1/posts/{id}/publish"),
                    &cookie,
                )
                .await;
                assert_eq!(res.status_code(), 202);
                assert_eq!(res.json::<Value>()["status"], "queued");
                let p = settled(&s, &cookie, &id).await;
                assert_eq!(p["status"], "published", "{p}");
                assert!(p["published_at"].is_string() && p["scheduled_at"].is_string());
                let t = &p["targets"][0];
                assert_eq!(t["status"], "published");
                let uri = t["remote_id"].as_str().unwrap();
                assert!(uri.starts_with("at://did:plc:"));
                assert!(t["remote_url"]
                    .as_str()
                    .unwrap()
                    .starts_with("https://bsky.app/profile/did:plc:"));
                let record = RECORDS.lock().unwrap().last().cloned().unwrap();
                assert_eq!(record["record"]["text"], text);
                let facet = &record["record"]["facets"][0];
                assert_eq!(facet["index"]["byteStart"], "🦀 read ".len());
                assert_eq!(facet["features"][0]["uri"], "https://example.com/a");
                assert!(record["record"].get("embed").is_none());

                let path = format!("/v1/posts/{id}");
                let res = req(&s, Method::PUT, &path, &cookie)
                    .json(&input(&set, "edit", None))
                    .await;
                assert_eq!(res.status_code(), 409);
                res.assert_json(&json!({"error": "not_editable"}));
                let res = req(&s, Method::POST, &format!("{path}/publish"), &cookie).await;
                assert_eq!(res.status_code(), 409, "published once only");

                let res = req(&s, Method::DELETE, &path, &cookie).await;
                res.assert_status_ok();
                let body = res.json::<Value>();
                assert_eq!(body["status"], "deleted");
                assert_eq!(body["targets"][0]["status"], "deleted");
                let res = req(&s, Method::DELETE, &path, &cookie).await;
                res.assert_status_ok();
            }

            #[tokio::test]
            #[serial]
            async fn the_tick_publishes_due_posts_with_images() {
                let (s, db, state) = on_mock().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let base = pds_of(&db, &team_id).await;
                let id =
                    id_of(&create(&s, &cookie, input(&set, "with images", Some(&past()))).await);
                set_media(&db, &base, &id, &["/img.png", "/img.png"]).await;
                let later = (Utc::now() + Duration::hours(1)).to_rfc3339();
                let not_due = id_of(&create(&s, &cookie, input(&set, "later", Some(&later))).await);
                publish::tick(&state).await.unwrap();
                let p = get_post(&s, &cookie, &id).await;
                assert_eq!(p["status"], "published", "{p}");
                let record = RECORDS.lock().unwrap().last().cloned().unwrap();
                let images = &record["record"]["embed"]["images"];
                assert_eq!(record["record"]["embed"]["$type"], "app.bsky.embed.images");
                assert_eq!(images.as_array().unwrap().len(), 2);
                assert_eq!(images[0]["alt"], "");
                assert_eq!(images[0]["image"]["mimeType"], "image/png");
                assert_eq!(get_post(&s, &cookie, &not_due).await["status"], "scheduled");
            }

            #[tokio::test]
            #[serial]
            async fn failures_are_scrubbed_and_name_the_account() {
                let (s, db, state) = on_mock().await;
                let (cookie, team_id, account) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let base = pds_of(&db, &team_id).await;
                let handle = list(&s, &cookie).await[0]["handle"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let cases = [
                    ("REJECT me", vec![], "Bluesky refused it (InvalidRequest)."),
                    (
                        "big",
                        vec!["/big.png"],
                        "Image 1 is over 1 MB, Bluesky's limit.",
                    ),
                    (
                        "html",
                        vec!["/page.html"],
                        "Image 1 is not a JPEG, PNG, WebP or GIF.",
                    ),
                ];
                for (text, media, msg) in cases {
                    let id = id_of(&create(&s, &cookie, input(&set, text, Some(&past()))).await);
                    set_media(&db, &base, &id, &media).await;
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "failed", "{text}");
                    assert_eq!(p["targets"][0]["error"], msg);
                    assert_eq!(p["error"], format!("@{handle}: {msg}"));
                    assert!(!p.to_string().contains("secret.example"));
                    let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                    assert_eq!(res.status_code(), 409);
                    res.assert_json(&json!({"error": "not_deletable"}));
                }

                // A refused app password asks for a reconnect, on publish and on takedown.
                let id = id_of(&create(&s, &cookie, input(&set, "fine", Some(&past()))).await);
                publish::tick(&state).await.unwrap();
                assert_eq!(get_post(&s, &cookie, &id).await["status"], "published");
                let row = social_accounts::ActiveModel {
                    id: Set(account.parse().unwrap()),
                    access_token: Set(cipher().encrypt(b"revoked-pass")),
                    ..Default::default()
                };
                social_accounts::Entity::update(row)
                    .exec(&db)
                    .await
                    .unwrap();
                let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                assert_eq!(res.status_code(), 422);
                let body = res.json::<Value>();
                assert_eq!(body["error"], "takedown_failed");
                assert!(body["message"]
                    .as_str()
                    .unwrap()
                    .contains("Reconnect the account"));
                assert_eq!(get_post(&s, &cookie, &id).await["status"], "published");
                let id = id_of(&create(&s, &cookie, input(&set, "after", Some(&past()))).await);
                publish::tick(&state).await.unwrap();
                let p = get_post(&s, &cookie, &id).await;
                assert_eq!(p["status"], "failed");
                assert_eq!(p["targets"][0]["error"], publish::RECONNECT);
            }

            #[tokio::test]
            #[serial]
            async fn a_failed_takedown_keeps_the_rest_for_a_retry() {
                let (s, db, state) = on_mock().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let id = id_of(&create(&s, &cookie, input(&set, "STICKY", Some(&past()))).await);
                publish::tick(&state).await.unwrap();
                let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                assert_eq!(res.status_code(), 422);
                let msg = res.json::<Value>()["message"].as_str().unwrap().to_owned();
                assert!(
                    msg.contains("InternalServerError") && msg.contains("Bluesky app"),
                    "{msg}"
                );
                let p = get_post(&s, &cookie, &id).await;
                assert_eq!(p["status"], "published");
                assert_eq!(p["targets"][0]["status"], "published");
            }

            #[tokio::test]
            #[serial]
            async fn claims_never_double_publish() {
                let (s, db, state) = on_mock().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let mut mine = Vec::new();
                for i in 0..5 {
                    let res =
                        create(&s, &cookie, input(&set, &format!("due {i}"), Some(&past()))).await;
                    mine.push(id_of(&res).parse::<uuid::Uuid>().unwrap());
                }
                let (a, b) = tokio::join!(publish::claim_due(&db), publish::claim_due(&db));
                let (a, b) = (a.unwrap(), b.unwrap());
                assert!(a.len() <= 50 && b.len() <= 50, "batches are capped");
                for id in &mine {
                    let n = a.iter().chain(&b).filter(|x| x.0 == *id).count();
                    assert_eq!(n, 1, "each due post is claimed once");
                }
                // Two runs of one queued post: every target is claimed once, one record each.
                let before = RECORDS.lock().unwrap().len();
                let (x, y) = tokio::join!(
                    publish::publish_post(&state, mine[0]),
                    publish::publish_post(&state, mine[0])
                );
                x.unwrap();
                y.unwrap();
                assert_eq!(RECORDS.lock().unwrap().len(), before + 1);
                let targets = post_targets::Entity::find()
                    .filter(post_targets::Column::PostId.eq(mine[0]))
                    .all(&db)
                    .await
                    .unwrap();
                assert_eq!(targets.len(), 1);
                let target = targets[0].id;
                let reset = post_targets::ActiveModel {
                    id: Set(target),
                    status: Set("pending".into()),
                    ..Default::default()
                };
                post_targets::Entity::update(reset).exec(&db).await.unwrap();
                let (p, q) = tokio::join!(
                    publish::claim_target(&db, target),
                    publish::claim_target(&db, target)
                );
                assert!(
                    !p.unwrap() && !q.unwrap(),
                    "a target with a remote id is never claimed"
                );
            }

            #[tokio::test]
            #[serial]
            async fn the_daily_quota_holds_back_publishes_and_past_times_mean_now() {
                use sea_orm::{ConnectionTrait, DbBackend, Statement};
                let (s, db, state) = on_mock().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let t0 = Utc::now() - Duration::seconds(1);
                let hour_ago = (Utc::now() - Duration::hours(1)).to_rfc3339();
                let res = create(&s, &cookie, input(&set, "past", Some(&hour_ago))).await;
                let p = res.json::<Value>();
                let at: chrono::DateTime<Utc> =
                    p["scheduled_at"].as_str().unwrap().parse().unwrap();
                assert!(at >= t0, "a past scheduled_at is clamped to now");
                let id = p["id"].as_str().unwrap().to_owned();

                let bucket = publish::quota_bucket(team_id.parse().unwrap());
                for _ in 0..publish::PUBLISHES_PER_DAY {
                    let sql = "INSERT INTO rate_limit_hits (bucket) VALUES ($1)";
                    let stmt = Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        sql,
                        [bucket.clone().into()],
                    );
                    db.execute_raw(stmt).await.unwrap();
                }
                publish::tick(&state).await.unwrap();
                assert_eq!(
                    get_post(&s, &cookie, &id).await["status"],
                    "scheduled",
                    "held back"
                );
                let draft = id_of(&create(&s, &cookie, input(&set, "now", None)).await);
                let res = req(
                    &s,
                    Method::POST,
                    &format!("/v1/posts/{draft}/publish"),
                    &cookie,
                )
                .await;
                assert_eq!(res.status_code(), 429);
                res.assert_json(&json!({"error": "publish_quota"}));
                assert_eq!(get_post(&s, &cookie, &draft).await["status"], "draft");
            }

            #[tokio::test]
            #[serial]
            async fn a_stuck_target_with_a_remote_id_is_published_and_can_come_down() {
                let (s, db, _) = on_mock().await;
                let (cookie, team_id, account) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let account: uuid::Uuid = account.parse().unwrap();
                let did = list(&s, &cookie).await[0]["external_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let post = queued_post(&db, &team_id, &set).await;
                let id = uuid::Uuid::now_v7();
                let uri = format!("at://{did}/app.bsky.feed.post/r3stuck");
                let row = post_targets::ActiveModel {
                    id: Set(id),
                    post_id: Set(post),
                    social_account_id: Set(Some(account)),
                    status: Set("publishing".into()),
                    remote_id: Set(Some(uri)),
                    updated_at: Set((Utc::now() - Duration::minutes(20)).fixed_offset()),
                    ..Default::default()
                };
                post_targets::Entity::insert(row)
                    .exec_without_returning(&db)
                    .await
                    .unwrap();
                publish::recover_stuck(&db).await.unwrap();
                let t = post_targets::Entity::find_by_id(id)
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(t.status, "published");
                assert_eq!(
                    t.remote_url.as_deref(),
                    Some(format!("https://bsky.app/profile/{did}/post/r3stuck").as_str())
                );
                let p = posts::Entity::find_by_id(post)
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(p.status, "published");
                let res = req(&s, Method::DELETE, &format!("/v1/posts/{post}"), &cookie).await;
                res.assert_status_ok();
                assert_eq!(res.json::<Value>()["status"], "deleted");
            }

            #[tokio::test]
            #[serial]
            async fn recovered_queued_posts_never_skip_the_quota() {
                use sea_orm::{ConnectionTrait, DbBackend, Statement};
                let (s, db, state) = on_mock().await;
                let (_, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let bucket = publish::quota_bucket(team_id.parse().unwrap());
                // Left `queued` 20 minutes ago with no reservation recorded.
                let stale = |id: uuid::Uuid| posts::ActiveModel {
                    id: Set(id),
                    updated_at: Set((Utc::now() - Duration::minutes(20)).fixed_offset()),
                    ..Default::default()
                };
                let first = queued_post(&db, &team_id, &set).await;
                posts::Entity::update(stale(first)).exec(&db).await.unwrap();
                let before = RECORDS.lock().unwrap().len();
                publish::tick(&state).await.unwrap();
                let p = posts::Entity::find_by_id(first)
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(p.status, "published");
                assert!(p.quota_reserved, "reserved before it went out");
                assert_eq!(hits(&db, &bucket).await, 1);
                assert_eq!(RECORDS.lock().unwrap().len(), before + 1);

                // At the cap, the recovered post goes back to scheduled and nothing is sent.
                for _ in 1..publish::PUBLISHES_PER_DAY {
                    let sql = "INSERT INTO rate_limit_hits (bucket) VALUES ($1)";
                    let stmt = Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        sql,
                        [bucket.clone().into()],
                    );
                    db.execute_raw(stmt).await.unwrap();
                }
                let second = queued_post(&db, &team_id, &set).await;
                posts::Entity::update(stale(second))
                    .exec(&db)
                    .await
                    .unwrap();
                publish::tick(&state).await.unwrap();
                let p = posts::Entity::find_by_id(second)
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(p.status, "scheduled");
                assert!(!p.quota_reserved);
                assert!(p.error.unwrap().contains("limit of 100"));
                assert_eq!(hits(&db, &bucket).await, publish::PUBLISHES_PER_DAY);
                assert_eq!(
                    RECORDS.lock().unwrap().len(),
                    before + 1,
                    "nothing more was sent"
                );
                let none = post_targets::Entity::find()
                    .filter(post_targets::Column::PostId.eq(second))
                    .count(&db)
                    .await
                    .unwrap();
                assert_eq!(none, 0);
            }

            #[tokio::test]
            #[serial]
            async fn a_tick_claims_at_most_50_posts() {
                let (s, db, _) = on_mock().await;
                let (_, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let mut mine = Vec::new();
                for _ in 0..51 {
                    let id = queued_post(&db, &team_id, &set).await;
                    let row = posts::ActiveModel {
                        id: Set(id),
                        status: Set("scheduled".into()),
                        ..Default::default()
                    };
                    posts::Entity::update(row).exec(&db).await.unwrap();
                    mine.push(id);
                }
                let first = publish::claim_due(&db).await.unwrap();
                assert_eq!(first.len(), 50);
                let mut claimed: Vec<uuid::Uuid> = first.iter().map(|c| c.0).collect();
                loop {
                    let next = publish::claim_due(&db).await.unwrap();
                    if next.is_empty() {
                        break;
                    }
                    claimed.extend(next.iter().map(|c| c.0));
                }
                for id in &mine {
                    assert_eq!(claimed.iter().filter(|c| *c == id).count(), 1);
                }
            }

            async fn queued_post(db: &DatabaseConnection, team: &str, set: &str) -> uuid::Uuid {
                let id = uuid::Uuid::now_v7();
                let row = posts::ActiveModel {
                    id: Set(id),
                    team_id: Set(team.parse().unwrap()),
                    social_set_id: Set(set.parse().unwrap()),
                    platform: Set("bluesky".into()),
                    body: Set("x".into()),
                    media: Set(json!([])),
                    status: Set("queued".into()),
                    scheduled_at: Set(Some(Utc::now().fixed_offset())),
                    ..Default::default()
                };
                posts::Entity::insert(row)
                    .exec_without_returning(db)
                    .await
                    .unwrap();
                id
            }

            async fn target(
                db: &DatabaseConnection,
                post: uuid::Uuid,
                account: Option<uuid::Uuid>,
                status: &str,
                updated: chrono::DateTime<Utc>,
            ) {
                let row = post_targets::ActiveModel {
                    id: Set(uuid::Uuid::now_v7()),
                    post_id: Set(post),
                    social_account_id: Set(account),
                    status: Set(status.into()),
                    error: Set((status == "failed").then(|| "Boom.".to_owned())),
                    updated_at: Set(updated.fixed_offset()),
                    ..Default::default()
                };
                post_targets::Entity::insert(row)
                    .exec_without_returning(db)
                    .await
                    .unwrap();
            }

            #[tokio::test]
            #[serial]
            async fn settle_races_agree_and_stuck_targets_fail_without_republishing() {
                let (s, db, state) = on_mock().await;
                let (_, team_id, account) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let account: uuid::Uuid = account.parse().unwrap();
                let post = queued_post(&db, &team_id, &set).await;
                target(&db, post, Some(account), "published", Utc::now()).await;
                target(&db, post, None, "failed", Utc::now()).await;
                let (a, b) = tokio::join!(publish::settle(&db, post), publish::settle(&db, post));
                a.unwrap();
                b.unwrap();
                let p = posts::Entity::find_by_id(post)
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    p.status, "published",
                    "partly failed still counts as published"
                );
                assert_eq!(p.error.as_deref(), Some("A disconnected account: Boom."));
                assert!(p.published_at.is_some());

                // A target cut off mid-publish 20 minutes ago fails and is never resent.
                let stuck = queued_post(&db, &team_id, &set).await;
                target(
                    &db,
                    stuck,
                    Some(account),
                    "publishing",
                    Utc::now() - Duration::minutes(20),
                )
                .await;
                let fresh = queued_post(&db, &team_id, &set).await;
                target(&db, fresh, Some(account), "publishing", Utc::now()).await;
                let before = RECORDS.lock().unwrap().len();
                publish::recover_stuck(&db).await.unwrap();
                publish::tick(&state).await.unwrap();
                let p = posts::Entity::find_by_id(stuck)
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(p.status, "failed");
                assert!(p.error.unwrap().contains("Unknown, check the account"));
                let f = posts::Entity::find_by_id(fresh)
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(f.status, "queued", "a recent publish is left alone");
                assert_eq!(
                    RECORDS.lock().unwrap().len(),
                    before,
                    "nothing was republished"
                );
            }

            #[tokio::test]
            #[serial]
            async fn deleted_sets_hold_posts_back() {
                let (s, db, state) = on_mock().await;
                let (cookie, _, _) = owner_with_account(&s, &db).await;
                let other = id_of(
                    &req(&s, Method::POST, "/v1/social-sets", &cookie)
                        .json(&json!({"name": "Held"}))
                        .await,
                );
                let id = id_of(&create(&s, &cookie, input(&other, "held", Some(&past()))).await);
                req(
                    &s,
                    Method::DELETE,
                    &format!("/v1/social-sets/{other}"),
                    &cookie,
                )
                .json(&json!({}))
                .await;
                publish::tick(&state).await.unwrap();
                let p = posts::Entity::find_by_id(id.parse::<uuid::Uuid>().unwrap())
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    p.status, "scheduled",
                    "not claimed while its set is deleted"
                );
                // A set deleted after the claim sends the post back to scheduled.
                let row = posts::ActiveModel {
                    id: Set(p.id),
                    status: Set("queued".into()),
                    ..Default::default()
                };
                posts::Entity::update(row).exec(&db).await.unwrap();
                publish::publish_post(&state, p.id).await.unwrap();
                let p = posts::Entity::find_by_id(p.id)
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(p.status, "scheduled");
                assert!(!p.quota_reserved, "it reserves again when claimed next");
                // An empty set fails the post with a message.
                req(
                    &s,
                    Method::POST,
                    &format!("/v1/social-sets/{other}/restore"),
                    &cookie,
                )
                .await;
                publish::tick(&state).await.unwrap();
                let p = get_post(&s, &cookie, &id).await;
                assert_eq!(p["status"], "failed");
                assert_eq!(p["error"], "No bluesky account is in this post's set.");
            }

            /// Publishes a text post with one image to the real Bluesky test account and deletes
            /// it. Needs BLUESKY_TEST_HANDLE and BLUESKY_TEST_APP_PASSWORD.
            #[tokio::test]
            #[serial]
            #[ignore = "calls the real Bluesky"]
            async fn live_bluesky_publish_and_takedown() {
                let handle = std::env::var("BLUESKY_TEST_HANDLE").expect("BLUESKY_TEST_HANDLE");
                let password =
                    std::env::var("BLUESKY_TEST_APP_PASSWORD").expect("BLUESKY_TEST_APP_PASSWORD");
                let (s, db) = server_where(|_| {}).await;
                let (_, cookie, _) = user(&s, &db).await;
                let team_id = current(&s, &cookie).await["id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let res = connect(&s, &cookie, &handle, &password).await;
                assert!(res.status_code().is_success(), "connect: {}", res.text());
                let set = default_set(&db, &team_id).await.to_string();
                let mut body = input(
                    &set,
                    "SocialReins live test, deleted right away: https://socialreins.com",
                    None,
                );
                body["media"] =
                    json!(["https://avatars.githubusercontent.com/u/5430905?s=200&v=4"]);
                let id = id_of(&create(&s, &cookie, body).await);
                let res = req(
                    &s,
                    Method::POST,
                    &format!("/v1/posts/{id}/publish"),
                    &cookie,
                )
                .await;
                assert_eq!(res.status_code(), 202);
                let p = settled(&s, &cookie, &id).await;
                assert_eq!(p["status"], "published", "{}", p["error"]);
                assert!(p["targets"][0]["remote_url"]
                    .as_str()
                    .unwrap()
                    .starts_with("https://bsky.app/profile/"));
                let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                assert_eq!(res.status_code(), 200, "{}", res.text());
                assert_eq!(res.json::<Value>()["status"], "deleted");
            }
        }

        /// `/v1/inboxes`, `/v1/inbox-items` and `/v1/automations`, with ingest driven directly
        /// against the mock (notifications, chat proxy, replies).
        mod inbox_api {
            use super::*;
            use socialreins_brain::{inbox, models::inbox_items};

            async fn on_mock() -> (TestServer, DatabaseConnection, AppState) {
                let base = mock_bluesky().await;
                let mut config = Config::from_env().expect("test config");
                config.trust_proxy = true;
                config.bluesky.appview = base.clone();
                config.bluesky.plc = base;
                config.bluesky.allow_insecure_pds = true;
                let db = socialreins_brain::connect(&config)
                    .await
                    .expect("database reachable");
                let state = AppState::new(db.clone(), &config);
                let app = controllers::router(state.clone(), &config);
                (TestServer::new(app).unwrap(), db, state)
            }

            /// An owner whose account has DM access: (cookie, team id, account id, DID).
            async fn owner(
                s: &TestServer,
                db: &DatabaseConnection,
            ) -> (String, String, String, String) {
                let (_, cookie, _) = user(s, db).await;
                let team_id = current(s, &cookie).await["id"].as_str().unwrap().to_owned();
                let res = connect(s, &cookie, &new_handle(), DM_APP_PASSWORD).await;
                assert_eq!(res.status_code(), 201);
                let body = res.json::<Value>();
                assert_eq!(body["dm_access"], true);
                let id = body["id"].as_str().unwrap().to_owned();
                (
                    cookie,
                    team_id,
                    id,
                    body["external_id"].as_str().unwrap().to_owned(),
                )
            }

            async fn ingest(db: &DatabaseConnection, state: &AppState, account: &str) {
                let a = social_accounts::Entity::find_by_id(account.parse::<uuid::Uuid>().unwrap())
                    .one(db)
                    .await
                    .unwrap()
                    .unwrap();
                inbox::ingest(state, a).await.unwrap();
            }

            fn at(ago_secs: i64) -> String {
                (Utc::now() - Duration::seconds(ago_secs))
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            }

            fn note(
                reason: &str,
                from: &str,
                rkey: &str,
                text: &str,
                when: &str,
                me: &str,
            ) -> Value {
                json!({
                    "uri": format!("at://{from}/app.bsky.feed.post/{rkey}"),
                    "cid": format!("cid-{rkey}"),
                    "reason": reason,
                    "author": {"did": from, "handle": format!("{rkey}.fan.test"),
                        "displayName": format!("Fan {rkey}")},
                    "record": {"text": text, "reply": {
                        "root": {"uri": format!("at://{me}/app.bsky.feed.post/root"), "cid": "cid-root"},
                        "parent": {"uri": format!("at://{me}/app.bsky.feed.post/root"), "cid": "cid-root"}}},
                    "indexedAt": when,
                })
            }

            async fn all_inbox(s: &TestServer, cookie: &str) -> Value {
                let res = s.get("/v1/inboxes").add_header("cookie", cookie).await;
                res.assert_status_ok();
                let list = res.json::<Value>();
                assert_eq!(list[0]["is_all"], true);
                list[0].clone()
            }

            async fn items(s: &TestServer, cookie: &str, inbox: &str, query: &str) -> Value {
                let res = s
                    .get(&format!("/v1/inboxes/{inbox}/items{query}"))
                    .add_header("cookie", cookie)
                    .await;
                res.assert_status_ok();
                res.json()
            }

            #[tokio::test]
            #[serial]
            async fn ingest_stores_items_once_and_inboxes_filter_them() {
                let (s, db, state) = on_mock().await;
                let (cookie, team_id, account, me) = owner(&s, &db).await;
                set_inbox(
                    &me,
                    json!({
                        "notifications": [
                            note("reply", "did:plc:fan1", "c1", "Love it, GUIDE please", &at(10), &me),
                            note("mention", "did:plc:fan2", "m1", "cc @me", &at(20), &me),
                            note("like", "did:plc:fan3", "l1", "", &at(30), &me),
                            note("reply", &me, "self", "my own reply", &at(40), &me),
                            note("reply", "did:plc:old", "o1", "too old", &at(8 * 86400), &me),
                        ],
                        "convos": [{"id": "cv1", "members": [
                            {"did": me, "handle": "me.test"},
                            {"did": "did:plc:dmer", "handle": "dmer.test", "displayName": "DMer"}],
                            "lastMessage": {"sentAt": at(5)}}],
                        "messages": {"cv1": [
                            {"$type": "chat.bsky.convo.defs#messageView", "id": "msg2",
                             "text": "thanks", "sender": {"did": me}, "sentAt": at(5)},
                            {"$type": "chat.bsky.convo.defs#messageView", "id": "msg1",
                             "text": "hello there", "sender": {"did": "did:plc:dmer"}, "sentAt": at(8)},
                        ]},
                    }),
                );
                ingest(&db, &state, &account).await;
                ingest(&db, &state, &account).await;
                let team: uuid::Uuid = team_id.parse().unwrap();
                let stored = inbox_items::Entity::find()
                    .filter(inbox_items::Column::TeamId.eq(team))
                    .count(&db)
                    .await
                    .unwrap();
                assert_eq!(stored, 3, "comment, mention and DM, each once");

                let all = all_inbox(&s, &cookie).await;
                let all_id = all["id"].as_str().unwrap();
                let page = items(&s, &cookie, all_id, "").await;
                let kinds: Vec<&str> = page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i["kind"].as_str().unwrap())
                    .collect();
                assert_eq!(kinds, ["dm", "comment", "mention"]);
                let dm = &page["items"][0];
                assert_eq!(dm["convo_id"], "cv1");
                assert_eq!(dm["author"]["name"], "DMer");
                let comment = &page["items"][1];
                assert_eq!(comment["author"]["handle"], "c1.fan.test");
                assert_eq!(
                    comment["root_remote_id"],
                    format!("at://{me}/app.bsky.feed.post/root")
                );
                assert!(comment["remote_url"]
                    .as_str()
                    .unwrap()
                    .starts_with("https://bsky.app/"));

                // Paging: one at a time, newest first.
                let p1 = items(&s, &cookie, all_id, "?limit=1").await;
                let cursor = p1["next_cursor"].as_str().unwrap();
                let p2 = items(&s, &cookie, all_id, &format!("?limit=2&cursor={cursor}")).await;
                assert_eq!(p2["items"][0]["kind"], "comment");
                assert!(p2["next_cursor"].is_null());
                let res = s
                    .get(&format!("/v1/inboxes/{all_id}/items?state=closed"))
                    .add_header("cookie", &cookie)
                    .await;
                assert_eq!(res.status_code(), 422);
                // Kind narrows the page server-side; an unknown kind is refused.
                let mentions = items(&s, &cookie, all_id, "?kind=mention").await;
                assert_eq!(mentions["items"].as_array().unwrap().len(), 1);
                assert_eq!(mentions["items"][0]["kind"], "mention");
                let res = s
                    .get(&format!("/v1/inboxes/{all_id}/items?kind=like"))
                    .add_header("cookie", &cookie)
                    .await;
                assert_eq!(res.status_code(), 422);

                // A filtered inbox: DMs and keyword matches only show where they match.
                let res = req(&s, Method::POST, "/v1/inboxes", &cookie)
                    .json(&json!({"name": "DMs", "filters": {"kinds": ["dm"],
                        "account_ids": [account]}}))
                    .await;
                assert_eq!(res.status_code(), 201, "{}", res.text());
                let dms = res.json::<Value>()["id"].as_str().unwrap().to_owned();
                let got = items(&s, &cookie, &dms, "").await;
                assert_eq!(got["items"].as_array().unwrap().len(), 1);
                let res = req(&s, Method::PUT, &format!("/v1/inboxes/{dms}"), &cookie)
                    .json(&json!({"name": "Guides", "filters": {"keywords": ["guide"]}}))
                    .await;
                assert_eq!(res.status_code(), 200);
                let got = items(&s, &cookie, &dms, "").await;
                assert_eq!(got["items"][0]["text"], "Love it, GUIDE please");
                assert_eq!(got["items"].as_array().unwrap().len(), 1);

                // Shapes and ids are checked; "All" stays.
                for (body, code) in [
                    (
                        json!({"name": "x", "filters": {"kinds": ["like"]}}),
                        "invalid_filters",
                    ),
                    (
                        json!({"name": "x", "filters": {"account_ids": [uuid::Uuid::now_v7()]}}),
                        "invalid_filters",
                    ),
                    (json!({"name": " ", "filters": {}}), "invalid_name"),
                    (
                        json!({"name": "x", "filters": {"kind": ["dm"]}}),
                        "invalid_body",
                    ),
                ] {
                    let res = req(&s, Method::POST, "/v1/inboxes", &cookie)
                        .json(&body)
                        .await;
                    assert_eq!(res.status_code(), 422, "{body}");
                    assert_eq!(res.json::<Value>()["error"], code, "{body}");
                }
                let res = req(
                    &s,
                    Method::DELETE,
                    &format!("/v1/inboxes/{all_id}"),
                    &cookie,
                )
                .await;
                assert_eq!(res.status_code(), 409);
                let res = req(&s, Method::PUT, &format!("/v1/inboxes/{all_id}"), &cookie)
                    .json(&json!({"name": "Mine"}))
                    .await;
                assert_eq!(res.status_code(), 409);

                // Members read and mark done, but do not manage inboxes; strangers see nothing.
                let (member, _) = joined(&s, &db, &team_id, "member").await;
                let res = req(&s, Method::POST, "/v1/inboxes", &member)
                    .json(&json!({"name": "Nope"}))
                    .await;
                assert_eq!(res.status_code(), 403);
                let item = comment["id"].as_str().unwrap();
                let res = req(
                    &s,
                    Method::PATCH,
                    &format!("/v1/inbox-items/{item}"),
                    &member,
                )
                .json(&json!({"state": "done"}))
                .await;
                assert_eq!(res.status_code(), 200);
                assert_eq!(res.json::<Value>()["state"], "done");
                let open = items(&s, &cookie, all_id, "?state=open").await;
                assert_eq!(open["items"].as_array().unwrap().len(), 2);
                let (_, stranger, _) = user(&s, &db).await;
                let res = s
                    .get(&format!("/v1/inbox-items/{item}"))
                    .add_header("cookie", &stranger)
                    .await;
                assert_eq!(res.status_code(), 404);
                let res = req(&s, Method::DELETE, &format!("/v1/inboxes/{dms}"), &cookie).await;
                assert_eq!(res.status_code(), 204);
            }

            #[tokio::test]
            #[serial]
            async fn replies_go_out_in_the_thread_or_the_conversation() {
                let (s, db, state) = on_mock().await;
                let (cookie, _, account, me) = owner(&s, &db).await;
                set_inbox(
                    &me,
                    json!({
                        "notifications": [note("reply", "did:plc:fan1", "c1", "hi", &at(10), &me)],
                        "convos": [{"id": "cv9", "members": [], "lastMessage": {"sentAt": at(5)}}],
                        "messages": {"cv9": [{"$type": "chat.bsky.convo.defs#messageView",
                            "id": "msg9", "text": "yo", "sender": {"did": "did:plc:dmer"},
                            "sentAt": at(5)}]},
                    }),
                );
                ingest(&db, &state, &account).await;
                let all = all_inbox(&s, &cookie).await;
                let page = items(&s, &cookie, all["id"].as_str().unwrap(), "").await;
                let dm = page["items"][0]["id"].as_str().unwrap().to_owned();
                let comment = page["items"][1]["id"].as_str().unwrap().to_owned();

                let res = req(
                    &s,
                    Method::POST,
                    &format!("/v1/inbox-items/{comment}/reply"),
                    &cookie,
                )
                .json(&json!({"text": "Thanks! https://socialreins.com"}))
                .await;
                assert_eq!(res.status_code(), 201, "{}", res.text());
                let item = res.json::<Value>();
                assert!(item["replied_at"].is_string());
                assert_eq!(item["replies"][0]["kind"], "public");
                assert!(item["replies"][0]["remote_url"]
                    .as_str()
                    .unwrap()
                    .starts_with("https://bsky.app/"));
                let record = RECORDS.lock().unwrap().last().cloned().unwrap();
                assert_eq!(
                    record["record"]["reply"]["parent"],
                    json!({"uri": "at://did:plc:fan1/app.bsky.feed.post/c1", "cid": "cid-c1"})
                );
                assert_eq!(
                    record["record"]["reply"]["root"],
                    json!({"uri": format!("at://{me}/app.bsky.feed.post/root"), "cid": "cid-root"})
                );

                let res = req(
                    &s,
                    Method::POST,
                    &format!("/v1/inbox-items/{dm}/reply"),
                    &cookie,
                )
                .json(&json!({"text": "Hello back"}))
                .await;
                assert_eq!(res.status_code(), 201, "{}", res.text());
                assert_eq!(res.json::<Value>()["replies"][0]["kind"], "dm");
                let sent = SENT.lock().unwrap().last().cloned().unwrap();
                assert_eq!(sent["convoId"], "cv9");
                assert_eq!(sent["message"]["text"], "Hello back");

                // Answered items drop out of an unanswered inbox.
                let res = req(&s, Method::POST, "/v1/inboxes", &cookie)
                    .json(&json!({"name": "Todo", "filters": {"unanswered": true}}))
                    .await;
                let todo = res.json::<Value>()["id"].as_str().unwrap().to_owned();
                assert_eq!(items(&s, &cookie, &todo, "").await["items"], json!([]));

                // What cannot go out says why, as 422 with a message.
                let res = req(
                    &s,
                    Method::POST,
                    &format!("/v1/inbox-items/{comment}/reply"),
                    &cookie,
                )
                .json(&json!({"text": "a".repeat(301)}))
                .await;
                assert_eq!(res.status_code(), 422);
                assert_eq!(res.json::<Value>()["error"], "invalid_text");
                let res = req(
                    &s,
                    Method::POST,
                    &format!("/v1/inbox-items/{dm}/reply"),
                    &cookie,
                )
                .json(&json!({"text": "BLOCKED"}))
                .await;
                assert_eq!(res.status_code(), 422);
                assert_eq!(res.json::<Value>()["error"], "dm_not_allowed");
                let row = social_accounts::ActiveModel {
                    id: Set(account.parse().unwrap()),
                    dm_access: Set(false),
                    ..Default::default()
                };
                social_accounts::Entity::update(row)
                    .exec(&db)
                    .await
                    .unwrap();
                let res = req(
                    &s,
                    Method::POST,
                    &format!("/v1/inbox-items/{dm}/reply"),
                    &cookie,
                )
                .json(&json!({"text": "again"}))
                .await;
                assert_eq!(res.status_code(), 422);
                let body = res.json::<Value>();
                assert_eq!(body["error"], "no_dm_access");
                assert!(body["message"]
                    .as_str()
                    .unwrap()
                    .contains("direct messages"));
                // The next poll sees the DM access again.
                ingest(&db, &state, &account).await;
                let res = s.get("/v1/connections").add_header("cookie", &cookie).await;
                assert_eq!(res.json::<Value>()[0]["dm_access"], true);
            }

            #[tokio::test]
            #[serial]
            async fn an_app_password_without_dm_access_still_gets_comments() {
                let (s, db, state) = on_mock().await;
                let (_, cookie, _) = user(&s, &db).await;
                let res = connect(&s, &cookie, &new_handle(), APP_PASSWORD).await;
                let body = res.json::<Value>();
                assert_eq!(body["dm_access"], false);
                let me = body["external_id"].as_str().unwrap().to_owned();
                set_inbox(
                    &me,
                    json!({"notifications": [
                    note("reply", "did:plc:fan1", "c1", "hi", &at(10), &me)]}),
                );
                ingest(&db, &state, body["id"].as_str().unwrap()).await;
                let all = all_inbox(&s, &cookie).await;
                let page = items(&s, &cookie, all["id"].as_str().unwrap(), "").await;
                assert_eq!(page["items"].as_array().unwrap().len(), 1);
            }

            fn comment_to_dm() -> Value {
                json!({
                    "name": "Guide by DM",
                    "trigger": "comment_received",
                    "conditions": {"keywords": ["guide"], "first_per_author": true},
                    "actions": [{"type": "send_dm", "text": "Hi {name}, here it is: https://x.io"}],
                    "fallback_public_reply": "{name}, open your DMs and comment again!",
                })
            }

            async fn runs(s: &TestServer, cookie: &str, id: &str) -> Vec<Value> {
                let res = s
                    .get(&format!("/v1/automations/{id}/runs"))
                    .add_header("cookie", cookie)
                    .await;
                res.assert_status_ok();
                res.json::<Value>()["runs"].as_array().unwrap().clone()
            }

            #[tokio::test]
            #[serial]
            async fn automations_are_strict_and_run_once_per_comment() {
                let (s, db, state) = on_mock().await;
                let (cookie, team_id, account, me) = owner(&s, &db).await;

                for (patch, code) in [
                    (json!({"trigger": "follow"}), "invalid_trigger"),
                    (json!({"actions": []}), "invalid_actions"),
                    (json!({"actions": [{"type": "like"}]}), "invalid_body"),
                    (json!({"conditions": {"keyword": ["x"]}}), "invalid_body"),
                    (
                        json!({"conditions": {"account_ids": [uuid::Uuid::now_v7()]}}),
                        "invalid_conditions",
                    ),
                    (
                        json!({"actions": [{"type": "reply_publicly", "text": "x"}]}),
                        "invalid_fallback",
                    ),
                    (json!({"surprise": true}), "invalid_body"),
                ] {
                    let mut body = comment_to_dm();
                    for (k, v) in patch.as_object().unwrap() {
                        body[k] = v.clone();
                    }
                    let res = req(&s, Method::POST, "/v1/automations", &cookie)
                        .json(&body)
                        .await;
                    assert_eq!(res.status_code(), 422, "{patch}");
                    assert_eq!(res.json::<Value>()["error"], code, "{patch}");
                }
                let (member, _) = joined(&s, &db, &team_id, "member").await;
                let res = req(&s, Method::POST, "/v1/automations", &member)
                    .json(&comment_to_dm())
                    .await;
                assert_eq!(res.status_code(), 403);

                let res = req(&s, Method::POST, "/v1/automations", &cookie)
                    .json(&comment_to_dm())
                    .await;
                assert_eq!(res.status_code(), 201, "{}", res.text());
                let auto = res.json::<Value>();
                assert_eq!(auto["active"], true);
                assert_eq!(auto["actions"][0]["type"], "send_dm");
                let id = auto["id"].as_str().unwrap().to_owned();
                // Members read automations and runs.
                let res = s.get("/v1/automations").add_header("cookie", &member).await;
                assert_eq!(res.json::<Value>().as_array().unwrap().len(), 1);

                set_inbox(
                    &me,
                    // Newest first, as Bluesky lists them.
                    json!({"notifications": [
                        note("reply", "did:plc:fan4", "c4", "nice", &at(-4), &me),
                        note("reply", "did:plc:closed1", "c3", "guide?", &at(-3), &me),
                        note("reply", "did:plc:fan1", "c2", "guide again", &at(-2), &me),
                        note("reply", "did:plc:fan1", "c1", "the GUIDE please", &at(-1), &me),
                    ]}),
                );
                let sent_before = SENT.lock().unwrap().len();
                ingest(&db, &state, &account).await;
                ingest(&db, &state, &account).await;
                let got = runs(&s, &member, &id).await;
                assert_eq!(got.len(), 2, "first comment of fan1, and closed1: {got:?}");
                let res = s.get("/v1/automations").add_header("cookie", &member).await;
                let listed = &res.json::<Value>()[0];
                assert_eq!(listed["runs"]["count"], 2);
                assert_eq!(listed["runs"]["last_at"], got[0]["created_at"]);
                // Newest first: closed1 got the fallback reply, fan1 the DM.
                assert_eq!(got[0]["status"], "sent", "{got:?}");
                assert_eq!(got[0]["reason"], "dm_not_allowed", "{got:?}");
                assert_eq!(got[0]["results"][1]["type"], "fallback_public_reply");
                assert_eq!(got[1]["status"], "sent");
                assert!(got[1]["reason"].is_null());
                let sent = SENT.lock().unwrap().clone();
                assert_eq!(sent.len(), sent_before + 1);
                assert_eq!(
                    sent.last().unwrap()["message"]["text"],
                    "Hi Fan c1, here it is: https://x.io"
                );
                assert_eq!(sent.last().unwrap()["convoId"], "convo-did:plc:fan1");
                let record = RECORDS.lock().unwrap().last().cloned().unwrap();
                assert_eq!(
                    record["record"]["text"],
                    "Fan c3, open your DMs and comment again!"
                );

                // Off: new comments do not run it; on again: only comments from then on.
                let res = req(
                    &s,
                    Method::POST,
                    &format!("/v1/automations/{id}/disable"),
                    &cookie,
                )
                .await;
                assert_eq!(res.json::<Value>()["active"], false);
                set_inbox(
                    &me,
                    json!({"notifications": [
                    note("reply", "did:plc:fan5", "c5", "guide", &at(-5), &me)]}),
                );
                ingest(&db, &state, &account).await;
                assert_eq!(runs(&s, &cookie, &id).await.len(), 2);
                let res = req(
                    &s,
                    Method::POST,
                    &format!("/v1/automations/{id}/enable"),
                    &cookie,
                )
                .await;
                assert_eq!(res.json::<Value>()["active"], true);

                // Past the daily cap a run is skipped, never sent later.
                let bucket = format!("automation:team:{team_id}");
                let stmt = sea_orm::Statement::from_sql_and_values(
                    sea_orm::DbBackend::Postgres,
                    "INSERT INTO rate_limit_hits (bucket) SELECT $1 FROM generate_series(1, 500)",
                    [bucket.clone().into()],
                );
                sea_orm::ConnectionTrait::execute_raw(&db, stmt)
                    .await
                    .unwrap();
                set_inbox(
                    &me,
                    json!({"notifications": [
                    note("reply", "did:plc:fan6", "c6", "guide", &at(-60), &me)]}),
                );
                ingest(&db, &state, &account).await;
                let got = runs(&s, &cookie, &id).await;
                assert_eq!(got.len(), 3);
                assert_eq!(got[0]["status"], "skipped");
                assert_eq!(got[0]["reason"], "daily_cap");
                let stmt = sea_orm::Statement::from_sql_and_values(
                    sea_orm::DbBackend::Postgres,
                    "DELETE FROM rate_limit_hits WHERE bucket = $1",
                    [bucket.into()],
                );
                sea_orm::ConnectionTrait::execute_raw(&db, stmt)
                    .await
                    .unwrap();

                // Replace and delete.
                let mut body = comment_to_dm();
                body["name"] = "Renamed".into();
                body["active"] = false.into();
                let res = req(&s, Method::PUT, &format!("/v1/automations/{id}"), &cookie)
                    .json(&body)
                    .await;
                assert_eq!(res.status_code(), 200, "{}", res.text());
                assert_eq!(res.json::<Value>()["name"], "Renamed");
                let res = req(
                    &s,
                    Method::DELETE,
                    &format!("/v1/automations/{id}"),
                    &member,
                )
                .await;
                assert_eq!(res.status_code(), 403);
                let res = req(
                    &s,
                    Method::DELETE,
                    &format!("/v1/automations/{id}"),
                    &cookie,
                )
                .await;
                assert_eq!(res.status_code(), 204);
            }

            #[tokio::test]
            #[serial]
            async fn the_tick_takes_the_lock_and_openapi_documents_the_routes() {
                let (s, _, state) = on_mock().await;
                inbox::tick(&state).await.unwrap();
                let doc = s.get("/openapi.json").await.json::<Value>();
                for (p, m) in [
                    ("/inboxes", "get"),
                    ("/inboxes", "post"),
                    ("/inboxes/{id}", "put"),
                    ("/inboxes/{id}", "delete"),
                    ("/inboxes/{id}/items", "get"),
                    ("/inbox-items/{id}", "get"),
                    ("/inbox-items/{id}", "patch"),
                    ("/inbox-items/{id}/reply", "post"),
                    ("/automations", "get"),
                    ("/automations", "post"),
                    ("/automations/{id}", "get"),
                    ("/automations/{id}", "put"),
                    ("/automations/{id}", "delete"),
                    ("/automations/{id}/enable", "post"),
                    ("/automations/{id}/disable", "post"),
                    ("/automations/{id}/runs", "get"),
                ] {
                    assert!(doc["paths"][p][m].is_object(), "{p} {m}");
                }
            }

            /// Reads the real Bluesky test account's notifications and DMs, read-only. Needs
            /// BLUESKY_TEST_HANDLE and BLUESKY_TEST_APP_PASSWORD.
            #[tokio::test]
            #[serial]
            #[ignore = "calls the real Bluesky"]
            async fn live_bluesky_ingest() {
                let handle = std::env::var("BLUESKY_TEST_HANDLE").expect("BLUESKY_TEST_HANDLE");
                let password =
                    std::env::var("BLUESKY_TEST_APP_PASSWORD").expect("BLUESKY_TEST_APP_PASSWORD");
                let config = Config::from_env().expect("test config");
                let db = socialreins_brain::connect(&config).await.unwrap();
                let state = AppState::new(db.clone(), &config);
                let s = TestServer::new(controllers::router(state.clone(), &config)).unwrap();
                let (_, cookie, _) = user(&s, &db).await;
                let res = connect(&s, &cookie, &handle, &password).await;
                assert!(res.status_code().is_success(), "connect: {}", res.text());
                let body = res.json::<Value>();
                let account = body["id"].as_str().unwrap().to_owned();
                ingest(&db, &state, &account).await;
                let a = social_accounts::Entity::find_by_id(account.parse::<uuid::Uuid>().unwrap())
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(a.inbox_error_at, None);
                assert_eq!(a.dm_access, body["dm_access"] == true);
            }
        }

        /// `/v1/api-tokens` and bearer access to `/v1/me`, sets, connections and posts.
        mod api_tokens_api {
            use super::*;
            use socialreins_brain::{models::api_tokens, rate_limit::RateLimiter};
            use std::sync::Arc;

            /// A brain on the mock whose API token limit is `per_minute`.
            async fn on_mock(per_minute: u32) -> (TestServer, DatabaseConnection) {
                let base = mock_bluesky().await;
                let mut config = Config::from_env().expect("test config");
                config.trust_proxy = true;
                config.bluesky.appview = base.clone();
                config.bluesky.plc = base;
                config.bluesky.allow_insecure_pds = true;
                let db = socialreins_brain::connect(&config)
                    .await
                    .expect("database reachable");
                let mut state = AppState::new(db.clone(), &config);
                state.api_token_limiter = Arc::new(RateLimiter::per_minute(per_minute));
                let app = controllers::router(state, &config);
                (TestServer::new(app).unwrap(), db)
            }

            async fn make_token(s: &TestServer, cookie: &str, body: Value) -> Value {
                let res = req(s, Method::POST, "/v1/api-tokens", cookie)
                    .json(&body)
                    .await;
                assert_eq!(res.status_code(), 201, "{}", res.text());
                res.json()
            }

            /// A request with `Authorization: Bearer <secret>` and no cookie or origin.
            fn bearer(s: &TestServer, method: Method, path: &str, secret: &str) -> TestRequest {
                s.method(method, path)
                    .add_header("authorization", format!("Bearer {secret}"))
            }

            fn post_body(set: Option<&str>, body: &str) -> Value {
                let mut v = json!({"platform": "bluesky", "body": body});
                if let Some(set) = set {
                    v["social_set_id"] = json!(set);
                }
                v
            }

            fn ids(list: &Value) -> Vec<String> {
                list.as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v["id"].as_str().unwrap().to_owned())
                    .collect()
            }

            #[tokio::test]
            #[serial]
            async fn create_shows_the_secret_once_and_only_managers_handle_tokens() {
                let (s, db) = on_mock(120).await;
                let (_, owner, _) = user(&s, &db).await;
                let team_id = current(&s, &owner).await["id"].as_str().unwrap().to_owned();
                let (member, _) = joined(&s, &db, &team_id, "member").await;

                let made = make_token(&s, &owner, json!({"name": "  Content Machine "})).await;
                let secret = made["secret"].as_str().unwrap().to_owned();
                assert!(secret.starts_with("sr_team_live_"), "{secret}");
                assert_eq!(secret.len(), "sr_team_live_".len() + 48);
                let t = &made["token"];
                assert_eq!(t["name"], "Content Machine");
                assert_eq!(t["social_set_id"], Value::Null);
                assert_eq!(t["prefix_hint"], secret[..17]);
                assert!(t.get("secret").is_none() && t.get("token_hash").is_none());
                let id = t["id"].as_str().unwrap().to_owned();

                let res = s.get("/v1/api-tokens").add_header("cookie", &owner).await;
                res.assert_status_ok();
                assert!(
                    !res.text().contains(&secret[13..]),
                    "list never shows the secret"
                );
                assert!(!res.text().contains("secret"));
                assert_eq!(ids(&res.json()), vec![id.clone()]);

                // Members cannot list, create or revoke.
                let res = s.get("/v1/api-tokens").add_header("cookie", &member).await;
                assert_eq!(res.status_code(), 403);
                let res = req(&s, Method::POST, "/v1/api-tokens", &member)
                    .json(&json!({"name": "x"}))
                    .await;
                assert_eq!(res.status_code(), 403);
                let path = format!("/v1/api-tokens/{id}");
                assert_eq!(
                    req(&s, Method::DELETE, &path, &member).await.status_code(),
                    403
                );

                // Validation, the web origin and a fresh sign-in.
                let (_, stranger, _) = user(&s, &db).await;
                let foreign = current(&s, &stranger).await["id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let foreign_set = default_set(&db, &foreign).await.to_string();
                for (body, code) in [
                    (json!({"name": " "}), "invalid_name"),
                    (json!({"name": "a".repeat(61)}), "invalid_name"),
                    (json!({"name": "x", "expires_in_days": 0}), "invalid_expiry"),
                    (
                        json!({"name": "x", "social_set_id": foreign_set}),
                        "invalid_set",
                    ),
                ] {
                    let res = req(&s, Method::POST, "/v1/api-tokens", &owner)
                        .json(&body)
                        .await;
                    assert_eq!(res.status_code(), 422, "{code}");
                    res.assert_json(&json!({ "error": code }));
                }
                let res = s
                    .post("/v1/api-tokens")
                    .add_header("cookie", &owner)
                    .json(&json!({"name": "x"}))
                    .await;
                res.assert_json(&json!({"error": "forbidden_origin"}));
                let (admin, _) = joined(&s, &db, &team_id, "admin").await;
                let row = sessions::ActiveModel {
                    token_hash: Set(token::hash(admin.trim_start_matches("sr_session="))),
                    created_at: Set((Utc::now() - Duration::minutes(11)).fixed_offset()),
                    ..Default::default()
                };
                sessions::Entity::update(row).exec(&db).await.unwrap();
                let res = req(&s, Method::POST, "/v1/api-tokens", &admin)
                    .json(&json!({"name": "x"}))
                    .await;
                res.assert_json(&json!({"error": "reauth_required"}));
                let res = s.get("/v1/api-tokens").add_header("cookie", &admin).await;
                res.assert_status_ok();

                // The token works, unknown ones do not, and a bearer header never falls back
                // to the cookie.
                let me = bearer(&s, Method::GET, "/v1/me", &secret).await;
                me.assert_status_ok();
                let me = me.json::<Value>();
                assert_eq!(me["team"]["id"], team_id.as_str());
                assert_eq!(me["team"]["role"], "admin");
                assert_eq!(me["token_name"], "Content Machine");
                let used = api_tokens::Entity::find_by_id(id.parse::<uuid::Uuid>().unwrap())
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(used.last_used_at.is_some());
                for bad in ["sr_team_live_nope", "nope", ""] {
                    let res = bearer(&s, Method::GET, "/v1/me", bad)
                        .add_header("cookie", &owner)
                        .await;
                    assert_eq!(res.status_code(), 401, "{bad}");
                    res.assert_json(&json!({"error": "invalid_token"}));
                }
                let res = s.get("/v1/me").add_header("cookie", &owner).await;
                assert_eq!(res.json::<Value>()["token_name"], Value::Null);

                // Revoked and expired tokens are refused and leave the list.
                assert_eq!(
                    req(&s, Method::DELETE, &path, &owner).await.status_code(),
                    204
                );
                assert_eq!(
                    req(&s, Method::DELETE, &path, &owner).await.status_code(),
                    404
                );
                let res = bearer(&s, Method::GET, "/v1/me", &secret).await;
                assert_eq!(res.status_code(), 401);
                res.assert_json(&json!({"error": "invalid_token"}));
                let later =
                    make_token(&s, &owner, json!({"name": "later", "expires_in_days": 1})).await;
                let later_secret = later["secret"].as_str().unwrap();
                bearer(&s, Method::GET, "/v1/me", later_secret)
                    .await
                    .assert_status_ok();
                let row = api_tokens::ActiveModel {
                    id: Set(later["token"]["id"].as_str().unwrap().parse().unwrap()),
                    expires_at: Set(Some((Utc::now() - Duration::seconds(1)).fixed_offset())),
                    ..Default::default()
                };
                api_tokens::Entity::update(row).exec(&db).await.unwrap();
                let res = bearer(&s, Method::GET, "/v1/me", later_secret).await;
                assert_eq!(res.status_code(), 401);
                let res = s.get("/v1/api-tokens").add_header("cookie", &owner).await;
                assert!(res.json::<Vec<Value>>().is_empty());

                // At most 20 live tokens.
                for i in 0..20 {
                    make_token(&s, &owner, json!({"name": format!("t{i}")})).await;
                }
                let res = req(&s, Method::POST, "/v1/api-tokens", &owner)
                    .json(&json!({"name": "one more"}))
                    .await;
                assert_eq!(res.status_code(), 409);
                res.assert_json(&json!({"error": "too_many_tokens"}));

                // Session-only routes refuse a token.
                let live = make_token(&s, &stranger, json!({"name": "s"})).await;
                let live = live["secret"].as_str().unwrap();
                for path in [
                    "/v1/inboxes",
                    "/v1/automations",
                    "/v1/team",
                    "/v1/api-tokens",
                    "/v1/connections/providers",
                ] {
                    let res = bearer(&s, Method::GET, path, live).await;
                    assert_eq!(res.status_code(), 401, "{path}");
                }
                let res = bearer(&s, Method::POST, "/v1/social-sets", live)
                    .json(&json!({"name": "nope"}))
                    .await;
                assert_eq!(
                    res.status_code(),
                    403,
                    "session writes still need the origin"
                );
            }

            #[tokio::test]
            #[serial]
            async fn set_tokens_see_and_write_only_their_set() {
                let (s, db) = on_mock(120).await;
                let (owner, team_id, first) = owner_with_account(&s, &db).await;
                let default = default_set(&db, &team_id).await.to_string();
                let clients = req(&s, Method::POST, "/v1/social-sets", &owner)
                    .json(&json!({"name": "Clients"}))
                    .await
                    .json::<Value>()["id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let res = connect(&s, &owner, &new_handle(), APP_PASSWORD).await;
                let second = res.json::<Value>()["id"].as_str().unwrap().to_owned();
                req(
                    &s,
                    Method::PUT,
                    &format!("/v1/connections/{second}/sets"),
                    &owner,
                )
                .json(&json!({"set_ids": [default, clients]}))
                .await
                .assert_status_ok();

                let team = make_token(&s, &owner, json!({"name": "team"})).await;
                let team = team["secret"].as_str().unwrap().to_owned();
                let set =
                    make_token(&s, &owner, json!({"name": "cm", "social_set_id": clients})).await;
                assert!(set["secret"].as_str().unwrap().starts_with("sr_set_live_"));
                let set = set["secret"].as_str().unwrap().to_owned();

                // A whole-team token must name the set; writes need no Origin header.
                let res = bearer(&s, Method::POST, "/v1/posts", &team)
                    .json(&post_body(None, "no set"))
                    .await;
                res.assert_json(&json!({"error": "invalid_set"}));
                let res = bearer(&s, Method::POST, "/v1/posts", &team)
                    .json(&post_body(Some(&default), "from the team token"))
                    .await;
                assert_eq!(res.status_code(), 201, "{}", res.text());
                let p = res.json::<Value>();
                assert_eq!(p["created_by"], Value::Null);
                let outside = p["id"].as_str().unwrap().to_owned();
                let res = bearer(&s, Method::PUT, &format!("/v1/posts/{outside}"), &team)
                    .json(&post_body(Some(&default), "edited"))
                    .await;
                res.assert_status_ok();

                // A set token: its set by default, never another.
                let me = bearer(&s, Method::GET, "/v1/me", &set)
                    .await
                    .json::<Value>();
                assert_eq!(me["social_set"]["id"], clients.as_str());
                assert_eq!(me["social_set"]["name"], "Clients");
                let res = bearer(&s, Method::POST, "/v1/posts", &set)
                    .json(&post_body(None, "mine"))
                    .await;
                assert_eq!(res.status_code(), 201, "{}", res.text());
                let mine = res.json::<Value>();
                assert_eq!(mine["social_set_id"], clients.as_str());
                let mine = mine["id"].as_str().unwrap().to_owned();
                let res = bearer(&s, Method::POST, "/v1/posts", &set)
                    .json(&post_body(Some(&clients), "mine too"))
                    .await;
                assert_eq!(res.status_code(), 201);
                let res = bearer(&s, Method::POST, "/v1/posts", &set)
                    .json(&post_body(Some(&default), "elsewhere"))
                    .await;
                assert_eq!(res.status_code(), 422);
                res.assert_json(&json!({"error": "invalid_set"}));
                let res = bearer(&s, Method::PUT, &format!("/v1/posts/{mine}"), &set)
                    .json(&post_body(Some(&default), "move it out"))
                    .await;
                res.assert_json(&json!({"error": "invalid_set"}));

                let list = |secret: String, q: &'static str| {
                    let s = &s;
                    async move {
                        let res = bearer(s, Method::GET, &format!("/v1/posts{q}"), &secret).await;
                        res.assert_status_ok();
                        res.json::<Value>()
                    }
                };
                assert_eq!(list(team.clone(), "").await.as_array().unwrap().len(), 3);
                let only = list(set.clone(), "").await;
                assert_eq!(only.as_array().unwrap().len(), 2);
                assert!(only
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|p| p["social_set_id"] == clients.as_str()));
                let q = Box::leak(format!("?social_set_id={default}").into_boxed_str());
                assert!(list(set.clone(), q).await.as_array().unwrap().is_empty());

                // Another set's post is missing: get, update, delete, publish.
                let path = format!("/v1/posts/{outside}");
                for (method, path) in [
                    (Method::GET, path.clone()),
                    (Method::PUT, path.clone()),
                    (Method::DELETE, path.clone()),
                    (Method::POST, format!("{path}/publish")),
                ] {
                    let res = bearer(&s, method.clone(), &path, &set)
                        .json(&post_body(None, "x"))
                        .await;
                    assert_eq!(res.status_code(), 404, "{method} {path}");
                    res.assert_json(&json!({"error": "not_found"}));
                }
                bearer(&s, Method::GET, &format!("/v1/posts/{mine}"), &set)
                    .await
                    .assert_status_ok();
                bearer(&s, Method::GET, &path, &team)
                    .await
                    .assert_status_ok();

                // Sets and connections are filtered to the token's set.
                let sets = bearer(&s, Method::GET, "/v1/social-sets", &set).await;
                assert_eq!(ids(&sets.json()), vec![clients.clone()]);
                let sets = bearer(&s, Method::GET, "/v1/social-sets", &team).await;
                assert_eq!(ids(&sets.json()).len(), 2);
                let conns = bearer(&s, Method::GET, "/v1/connections", &set)
                    .await
                    .json::<Value>();
                assert_eq!(ids(&conns), vec![second.clone()]);
                assert_eq!(conns[0]["set_ids"], json!([clients]));
                let conns = bearer(&s, Method::GET, "/v1/connections", &team)
                    .await
                    .json::<Value>();
                assert_eq!(ids(&conns), vec![first, second]);

                // The set token may delete its own draft.
                let res = bearer(&s, Method::DELETE, &format!("/v1/posts/{mine}"), &set).await;
                assert_eq!(res.status_code(), 204);

                // Deleting the set revokes its tokens; a restore does not bring them back.
                req(
                    &s,
                    Method::DELETE,
                    &format!("/v1/social-sets/{clients}"),
                    &owner,
                )
                .await
                .assert_status(axum::http::StatusCode::NO_CONTENT);
                let res = bearer(&s, Method::GET, "/v1/me", &set).await;
                assert_eq!(res.status_code(), 401);
                req(
                    &s,
                    Method::POST,
                    &format!("/v1/social-sets/{clients}/restore"),
                    &owner,
                )
                .await
                .assert_status_ok();
                let res = bearer(&s, Method::GET, "/v1/me", &set).await;
                assert_eq!(res.status_code(), 401);
                bearer(&s, Method::GET, "/v1/me", &team)
                    .await
                    .assert_status_ok();
            }

            #[tokio::test]
            #[serial]
            async fn tokens_are_rate_limited_per_token() {
                let (s, db) = on_mock(3).await;
                let (_, owner, _) = user(&s, &db).await;
                let a = make_token(&s, &owner, json!({"name": "a"})).await;
                let b = make_token(&s, &owner, json!({"name": "b"})).await;
                let (a, b) = (a["secret"].as_str().unwrap(), b["secret"].as_str().unwrap());
                for _ in 0..3 {
                    bearer(&s, Method::GET, "/v1/posts", a)
                        .await
                        .assert_status_ok();
                }
                let res = bearer(&s, Method::GET, "/v1/posts", a).await;
                assert_eq!(res.status_code(), 429);
                res.assert_json(&json!({"error": "rate_limited"}));
                let wait: u64 = res.header("retry-after").to_str().unwrap().parse().unwrap();
                assert!((1..=60).contains(&wait), "{wait}");
                bearer(&s, Method::GET, "/v1/posts", b)
                    .await
                    .assert_status_ok();
                // Sessions are not counted against tokens.
                for _ in 0..5 {
                    s.get("/v1/posts")
                        .add_header("cookie", &owner)
                        .await
                        .assert_status_ok();
                }
            }

            #[tokio::test]
            #[serial]
            async fn openapi_documents_the_routes_and_the_bearer_scheme() {
                let (s, _) = on_mock(120).await;
                let doc = s.get("/openapi.json").await.json::<Value>();
                for (p, m) in [
                    ("/me", "get"),
                    ("/api-tokens", "get"),
                    ("/api-tokens", "post"),
                    ("/api-tokens/{id}", "delete"),
                ] {
                    assert!(doc["paths"][p][m].is_object(), "{p} {m}");
                }
                let schemes = &doc["components"]["securitySchemes"];
                assert_eq!(schemes["api_token"]["scheme"], "bearer");
                assert_eq!(schemes["session"]["in"], "cookie");
                for (p, m) in [
                    ("/me", "get"),
                    ("/social-sets", "get"),
                    ("/connections", "get"),
                    ("/posts", "get"),
                    ("/posts", "post"),
                    ("/posts/{id}", "get"),
                    ("/posts/{id}", "put"),
                    ("/posts/{id}", "delete"),
                    ("/posts/{id}/publish", "post"),
                ] {
                    let sec = doc["paths"][p][m]["security"].to_string();
                    assert!(sec.contains("api_token"), "{p} {m} {sec}");
                }
                for (p, m) in [
                    ("/posts", "post"),
                    ("/posts/{id}", "put"),
                    ("/posts/{id}", "delete"),
                    ("/posts/{id}/publish", "post"),
                ] {
                    let params = doc["paths"][p][m]["parameters"].to_string();
                    assert!(params.contains("\"Idempotency-Key\""), "{p} {m} {params}");
                }
                for (p, m) in [
                    ("/api-tokens", "get"),
                    ("/inboxes", "get"),
                    ("/team", "get"),
                ] {
                    assert!(doc["paths"][p][m]["security"].is_null(), "{p} {m}");
                }
            }

            /// `/mcp`: the same tokens, scope and code as `/v1`.
            mod mcp {
                use super::*;
                use axum_test::TestResponse;

                async fn rpc(
                    s: &TestServer,
                    secret: Option<&str>,
                    method: &str,
                    params: Value,
                ) -> TestResponse {
                    let mut r = s
                        .post("/mcp")
                        .add_header("accept", "application/json, text/event-stream")
                        .add_header("mcp-protocol-version", "2025-11-25");
                    if let Some(t) = secret {
                        r = r.add_header("authorization", format!("Bearer {t}"));
                    }
                    r.json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
                        .await
                }

                /// A tool call: `(isError, the JSON the tool answered as text)`.
                pub(super) async fn call(
                    s: &TestServer,
                    secret: &str,
                    tool: &str,
                    args: Value,
                ) -> (bool, Value) {
                    let params = json!({"name": tool, "arguments": args});
                    let res = rpc(s, Some(secret), "tools/call", params).await;
                    assert_eq!(res.status_code(), 200, "{}", res.text());
                    let body = res.json::<Value>();
                    let r = &body["result"];
                    let text = r["content"][0]["text"].as_str().expect("text content");
                    (r["isError"] == true, serde_json::from_str(text).unwrap())
                }

                #[tokio::test]
                #[serial]
                async fn mcp_needs_a_live_token_not_a_cookie() {
                    let (s, db) = on_mock(120).await;
                    let (_, owner, _) = user(&s, &db).await;
                    let made = make_token(&s, &owner, json!({"name": "m"})).await;
                    let secret = made["secret"].as_str().unwrap();
                    for bad in [None, Some("sr_team_live_nope"), Some("nope")] {
                        let res = rpc(&s, bad, "tools/list", json!({})).await;
                        assert_eq!(res.status_code(), 401, "{bad:?}");
                        res.assert_json(&json!({"error": "invalid_token"}));
                        assert_eq!(res.header("www-authenticate"), "Bearer");
                    }
                    let res = s
                        .post("/mcp")
                        .add_header("cookie", &owner)
                        .add_header("accept", "application/json, text/event-stream")
                        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
                        .await;
                    assert_eq!(res.status_code(), 401, "a session alone is refused");
                    let id = made["token"]["id"].as_str().unwrap();
                    req(&s, Method::DELETE, &format!("/v1/api-tokens/{id}"), &owner).await;
                    let res = rpc(&s, Some(secret), "tools/list", json!({})).await;
                    assert_eq!(res.status_code(), 401, "revoked");
                }

                #[tokio::test]
                #[serial]
                async fn initialize_lists_the_tools_and_they_match_rest() {
                    let (s, db) = on_mock(120).await;
                    let (owner, team_id, _) = owner_with_account(&s, &db).await;
                    let default = default_set(&db, &team_id).await.to_string();
                    let made = make_token(&s, &owner, json!({"name": "m"})).await;
                    let secret = made["secret"].as_str().unwrap();

                    let init = json!({
                        "protocolVersion": "2025-11-25",
                        "capabilities": {},
                        "clientInfo": {"name": "test", "version": "1"},
                    });
                    let res = rpc(&s, Some(secret), "initialize", init).await;
                    assert_eq!(res.status_code(), 200, "{}", res.text());
                    let body = res.json::<Value>();
                    assert_eq!(
                        body["result"]["serverInfo"]["name"], "socialreins",
                        "{body}"
                    );
                    let res = rpc(&s, Some(secret), "tools/list", json!({})).await;
                    let body = res.json::<Value>();
                    let mut names: Vec<&str> = body["result"]["tools"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|t| t["name"].as_str().unwrap())
                        .collect();
                    names.sort_unstable();
                    assert_eq!(
                        names,
                        [
                            "add-post-to-list",
                            "create-list",
                            "create-post",
                            "delete-post",
                            "get-list",
                            "get-post",
                            "list-lists",
                            "list-posts",
                            "list-social-accounts",
                            "list-social-sets",
                            "remove-post-from-list",
                            "reorder-list",
                            "update-post",
                        ]
                    );

                    let (err, p) = call(
                        &s,
                        secret,
                        "create-post",
                        post_body(Some(&default), "via mcp"),
                    )
                    .await;
                    assert!(!err, "{p}");
                    let id = p["id"].as_str().unwrap().to_owned();
                    let rest = bearer(&s, Method::GET, &format!("/v1/posts/{id}"), secret).await;
                    assert_eq!(rest.json::<Value>(), p);
                    let (_, got) = call(&s, secret, "get-post", json!({"id": id})).await;
                    assert_eq!(got, p);
                    let (_, list) = call(&s, secret, "list-posts", json!({})).await;
                    let rest = bearer(&s, Method::GET, "/v1/posts", secret).await;
                    assert_eq!(list, rest.json::<Value>());
                    let (_, sets) = call(&s, secret, "list-social-sets", json!({})).await;
                    let rest = bearer(&s, Method::GET, "/v1/social-sets", secret).await;
                    assert_eq!(sets, rest.json::<Value>());
                    let (_, accounts) = call(&s, secret, "list-social-accounts", json!({})).await;
                    let rest = bearer(&s, Method::GET, "/v1/connections", secret).await;
                    assert_eq!(accounts, rest.json::<Value>());

                    let mut edit = post_body(Some(&default), "edited");
                    edit["id"] = json!(id);
                    let (err, p) = call(&s, secret, "update-post", edit).await;
                    assert!(!err && p["body"] == "edited", "{p}");

                    // REST's 422s come back as isError with the same JSON.
                    for (args, code) in [
                        (post_body(Some(&default), ""), "empty_post"),
                        (json!({"body": "no platform"}), "invalid_body"),
                    ] {
                        let (err, e) = call(&s, secret, "create-post", args).await;
                        assert!(err);
                        assert_eq!(e, json!({"error": code}));
                    }
                    let (err, e) = call(&s, secret, "list-posts", json!({"status": "nope"})).await;
                    assert!(err);
                    assert_eq!(e, json!({"error": "invalid_query"}));

                    let (err, _) = call(&s, secret, "delete-post", json!({"id": id})).await;
                    assert!(!err);
                    let (err, e) = call(&s, secret, "get-post", json!({"id": id})).await;
                    assert!(err);
                    assert_eq!(e, json!({"error": "not_found"}));
                }

                #[tokio::test]
                #[serial]
                async fn set_tokens_stay_in_their_set() {
                    let (s, db) = on_mock(120).await;
                    let (owner, team_id, _) = owner_with_account(&s, &db).await;
                    let default = default_set(&db, &team_id).await.to_string();
                    let clients = req(&s, Method::POST, "/v1/social-sets", &owner)
                        .json(&json!({"name": "Clients"}))
                        .await
                        .json::<Value>()["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    let team = make_token(&s, &owner, json!({"name": "team"})).await;
                    let team = team["secret"].as_str().unwrap();
                    let set =
                        make_token(&s, &owner, json!({"name": "cm", "social_set_id": clients}))
                            .await;
                    let set = set["secret"].as_str().unwrap();
                    let res = connect(&s, &owner, &new_handle(), APP_PASSWORD).await;
                    let second = res.json::<Value>()["id"].as_str().unwrap().to_owned();
                    req(
                        &s,
                        Method::PUT,
                        &format!("/v1/connections/{second}/sets"),
                        &owner,
                    )
                    .json(&json!({"set_ids": [default, clients]}))
                    .await
                    .assert_status_ok();
                    let (_, other) = call(
                        &s,
                        team,
                        "create-post",
                        post_body(Some(&default), "elsewhere"),
                    )
                    .await;
                    let other = other["id"].as_str().unwrap();

                    let (_, sets) = call(&s, set, "list-social-sets", json!({})).await;
                    assert_eq!(ids(&sets), vec![clients.clone()]);
                    let (err, e) = call(&s, set, "get-post", json!({"id": other})).await;
                    assert!(err);
                    assert_eq!(e, json!({"error": "not_found"}));
                    let (err, e) =
                        call(&s, set, "create-post", post_body(Some(&default), "no")).await;
                    assert!(err);
                    assert_eq!(e, json!({"error": "invalid_set"}));
                    let (err, p) = call(&s, set, "create-post", post_body(None, "mine")).await;
                    assert!(!err);
                    assert_eq!(p["social_set_id"], clients.as_str());
                    let mine = p["id"].as_str().unwrap().to_owned();

                    // Another set's post is not found for writes either.
                    let mut edit = post_body(None, "hijack");
                    edit["id"] = json!(other);
                    let (err, e) = call(&s, set, "update-post", edit).await;
                    assert!(err);
                    assert_eq!(e, json!({"error": "not_found"}));
                    let (err, e) = call(&s, set, "delete-post", json!({"id": other})).await;
                    assert!(err);
                    assert_eq!(e, json!({"error": "not_found"}));
                    // Moving its own post out of its set is invalid_set, as on REST.
                    let mut moved = post_body(Some(&default), "moved");
                    moved["id"] = json!(mine);
                    let (err, e) = call(&s, set, "update-post", moved).await;
                    assert!(err);
                    assert_eq!(e, json!({"error": "invalid_set"}));

                    // Lists hold only its set.
                    let (_, list) = call(&s, set, "list-posts", json!({})).await;
                    assert_eq!(ids(&list), vec![mine.clone()]);
                    let (_, accounts) = call(&s, set, "list-social-accounts", json!({})).await;
                    assert_eq!(ids(&accounts), vec![second.clone()]);
                    assert_eq!(accounts[0]["set_ids"], json!([clients]));

                    // Another team's token cannot read it.
                    let (_, stranger, _) = user(&s, &db).await;
                    let theirs = make_token(&s, &stranger, json!({"name": "x"})).await;
                    let theirs = theirs["secret"].as_str().unwrap();
                    let (err, e) = call(&s, theirs, "get-post", json!({"id": mine})).await;
                    assert!(err);
                    assert_eq!(e, json!({"error": "not_found"}));
                }

                /// Over `MAX_BODY_BYTES` is 413, with a length header or chunked without one.
                #[tokio::test]
                #[serial]
                async fn mcp_bodies_over_4_kib_are_refused() {
                    use std::io::{Read, Write};
                    let (s, db) = on_mock(120).await;
                    let (_, owner, _) = user(&s, &db).await;
                    let made = make_token(&s, &owner, json!({"name": "m"})).await;
                    let secret = made["secret"].as_str().unwrap().to_owned();
                    let big =
                        json!({"name": "create-post", "arguments": {"body": "a".repeat(5000)}});
                    let res = rpc(&s, Some(&secret), "tools/call", big.clone()).await;
                    assert_eq!(res.status_code(), 413, "{}", res.text());

                    let config = Config::from_env().expect("test config");
                    let app = controllers::router(AppState::new(db.clone(), &config), &config);
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let addr = listener.local_addr().unwrap();
                    tokio::spawn(async move { axum::serve(listener, app).await });
                    let body =
                        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": big})
                            .to_string();
                    let reply = tokio::task::spawn_blocking(move || {
                        let mut c = std::net::TcpStream::connect(addr).unwrap();
                        let head = format!(
                            "POST /mcp HTTP/1.1\r\nhost: localhost\r\n\
                             authorization: Bearer {secret}\r\n\
                             content-type: application/json\r\n\
                             accept: application/json, text/event-stream\r\n\
                             transfer-encoding: chunked\r\nconnection: close\r\n\r\n"
                        );
                        c.write_all(head.as_bytes()).unwrap();
                        for part in body.as_bytes().chunks(1000) {
                            let _ = c.write_all(format!("{:x}\r\n", part.len()).as_bytes());
                            let _ = c.write_all(part);
                            let _ = c.write_all(b"\r\n");
                        }
                        // The server may answer and close before the last chunk.
                        let _ = c.write_all(b"0\r\n\r\n");
                        let mut out = String::new();
                        let _ = c.read_to_string(&mut out);
                        out
                    })
                    .await
                    .unwrap();
                    assert!(reply.starts_with("HTTP/1.1 413"), "{reply}");
                }

                #[tokio::test]
                #[serial]
                async fn mcp_shares_the_token_limit_with_v1() {
                    let (s, db) = on_mock(2).await;
                    let (_, owner, _) = user(&s, &db).await;
                    let made = make_token(&s, &owner, json!({"name": "m"})).await;
                    let secret = made["secret"].as_str().unwrap();
                    bearer(&s, Method::GET, "/v1/posts", secret)
                        .await
                        .assert_status_ok();
                    let res = rpc(&s, Some(secret), "tools/list", json!({})).await;
                    assert_eq!(res.status_code(), 200, "{}", res.text());
                    let res = rpc(&s, Some(secret), "tools/list", json!({})).await;
                    assert_eq!(res.status_code(), 429);
                    res.assert_json(&json!({"error": "rate_limited"}));
                }
            }

            /// Post lists: order, scope and the `lists` of every post.
            mod lists {
                use super::*;
                use sea_orm::{ConnectionTrait, DbBackend, Statement};

                /// Owner cookie, Default set, a "Clients" set, a team token and a Clients token.
                async fn setup(
                    s: &TestServer,
                    db: &DatabaseConnection,
                ) -> (String, String, String, String, String) {
                    let (_, owner, _) = user(s, db).await;
                    let team_id = current(s, &owner).await["id"].as_str().unwrap().to_owned();
                    let default = default_set(db, &team_id).await.to_string();
                    let clients = req(s, Method::POST, "/v1/social-sets", &owner)
                        .json(&json!({"name": "Clients"}))
                        .await
                        .json::<Value>()["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    let team = make_token(s, &owner, json!({"name": "team"})).await;
                    let set =
                        make_token(s, &owner, json!({"name": "cm", "social_set_id": clients}))
                            .await;
                    let secret = |v: Value| v["secret"].as_str().unwrap().to_owned();
                    (owner, default, clients, secret(team), secret(set))
                }

                async fn new_post(s: &TestServer, secret: &str, set: &str) -> String {
                    let res = bearer(s, Method::POST, "/v1/posts", secret)
                        .json(&post_body(Some(set), "part"))
                        .await;
                    assert_eq!(res.status_code(), 201, "{}", res.text());
                    res.json::<Value>()["id"].as_str().unwrap().to_owned()
                }

                async fn new_list(s: &TestServer, secret: &str, body: Value) -> String {
                    let res = bearer(s, Method::POST, "/v1/lists", secret)
                        .json(&body)
                        .await;
                    assert_eq!(res.status_code(), 201, "{}", res.text());
                    res.json::<Value>()["id"].as_str().unwrap().to_owned()
                }

                async fn add(
                    s: &TestServer,
                    secret: &str,
                    list: &str,
                    body: Value,
                ) -> axum_test::TestResponse {
                    bearer(s, Method::POST, &format!("/v1/lists/{list}/posts"), secret)
                        .json(&body)
                        .await
                }

                /// `(post id, position)` of each item the caller sees.
                async fn items(s: &TestServer, secret: &str, list: &str) -> Vec<(String, i64)> {
                    let res = bearer(s, Method::GET, &format!("/v1/lists/{list}"), secret).await;
                    assert_eq!(res.status_code(), 200, "{}", res.text());
                    let body = res.json::<Value>();
                    body["items"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|i| {
                            assert_eq!(i["post"]["id"], i["post_id"]);
                            let id = i["post_id"].as_str().unwrap().to_owned();
                            (id, i["position"].as_i64().unwrap())
                        })
                        .collect()
                }

                fn ranked(ids: &[&String]) -> Vec<(String, i64)> {
                    ids.iter()
                        .enumerate()
                        .map(|(i, id)| ((*id).clone(), i as i64 + 1))
                        .collect()
                }

                async fn reorder(
                    s: &TestServer,
                    secret: &str,
                    list: &str,
                    ids: &[&String],
                ) -> axum_test::TestResponse {
                    bearer(s, Method::PUT, &format!("/v1/lists/{list}/order"), secret)
                        .json(&json!({"post_ids": ids}))
                        .await
                }

                async fn post_lists(s: &TestServer, secret: &str, post: &str) -> Value {
                    let res = bearer(s, Method::GET, &format!("/v1/posts/{post}"), secret).await;
                    assert_eq!(res.status_code(), 200, "{}", res.text());
                    res.json::<Value>()["lists"].clone()
                }

                #[tokio::test]
                #[serial]
                async fn lists_keep_order_and_posts_carry_them() {
                    let (s, db) = on_mock(1000).await;
                    let (owner, default, clients, team, _) = setup(&s, &db).await;
                    let (p1, p2, p3) = (
                        new_post(&s, &team, &default).await,
                        new_post(&s, &team, &default).await,
                        new_post(&s, &team, &default).await,
                    );

                    // A session creates a team-wide list.
                    let res = req(&s, Method::POST, "/v1/lists", &owner)
                        .json(&json!({"name": "  Series  "}))
                        .await;
                    assert_eq!(res.status_code(), 201, "{}", res.text());
                    let made = res.json::<Value>();
                    assert_eq!(made["name"], "Series");
                    assert_eq!(made["social_set_id"], Value::Null);
                    assert_eq!(made["remote_ids"], json!({}));
                    assert_eq!(made["items"], json!([]));
                    let list = made["id"].as_str().unwrap().to_owned();

                    // Adding appends; adding again moves.
                    for p in [&p1, &p2, &p3] {
                        add(&s, &team, &list, json!({"post_id": p}))
                            .await
                            .assert_status_ok();
                    }
                    assert_eq!(items(&s, &team, &list).await, ranked(&[&p1, &p2, &p3]));
                    let res = add(&s, &team, &list, json!({"post_id": p3, "position": 1})).await;
                    res.assert_status_ok();
                    assert_eq!(res.json::<Value>()["items"][0]["post_id"], p3.as_str());
                    assert_eq!(items(&s, &team, &list).await, ranked(&[&p3, &p1, &p2]));
                    let res = add(&s, &team, &list, json!({"post_id": p3, "position": 9})).await;
                    res.assert_status_ok();
                    assert_eq!(items(&s, &team, &list).await, ranked(&[&p1, &p2, &p3]));
                    let res = add(&s, &team, &list, json!({"post_id": p3, "position": 0})).await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json(&json!({"error": "invalid_position"}));
                    let res = add(&s, &team, &list, json!({"post_id": uuid::Uuid::now_v7()})).await;
                    res.assert_json(&json!({"error": "invalid_post"}));

                    // Every post carries its lists, in the single and the list answer.
                    let want = json!([{"id": list, "name": "Series", "position": 2}]);
                    assert_eq!(post_lists(&s, &team, &p2).await, want);
                    let all = bearer(&s, Method::GET, "/v1/posts", &team)
                        .await
                        .json::<Value>();
                    let p2_json = all
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|p| p["id"] == p2.as_str());
                    assert_eq!(p2_json.unwrap()["lists"], want);

                    // Removing compacts.
                    let path = format!("/v1/lists/{list}/posts/{p1}");
                    let res = bearer(&s, Method::DELETE, &path, &team).await;
                    assert_eq!(res.status_code(), 204);
                    assert_eq!(items(&s, &team, &list).await, ranked(&[&p2, &p3]));
                    let res = bearer(&s, Method::DELETE, &path, &team).await;
                    assert_eq!(res.status_code(), 404);
                    assert_eq!(post_lists(&s, &team, &p1).await, json!([]));

                    // A reorder names every post once.
                    reorder(&s, &team, &list, &[&p3, &p2])
                        .await
                        .assert_status_ok();
                    assert_eq!(items(&s, &team, &list).await, ranked(&[&p3, &p2]));
                    for bad in [vec![&p3], vec![&p3, &p3], vec![&p3, &p2, &p1], vec![]] {
                        let res = reorder(&s, &team, &list, &bad).await;
                        assert_eq!(res.status_code(), 422, "{bad:?}");
                        res.assert_json(&json!({"error": "invalid_order"}));
                    }

                    // Rename.
                    let res = bearer(&s, Method::PATCH, &format!("/v1/lists/{list}"), &team)
                        .json(&json!({"name": "Season 1"}))
                        .await;
                    res.assert_status_ok();
                    assert_eq!(res.json::<Value>()["name"], "Season 1");
                    let res = bearer(&s, Method::PATCH, &format!("/v1/lists/{list}"), &team)
                        .json(&json!({"name": " "}))
                        .await;
                    res.assert_json(&json!({"error": "invalid_name"}));

                    // A post is created into a list, and moved into another by PUT.
                    let mut body = post_body(Some(&default), "new part");
                    body["list_id"] = json!(list);
                    body["position"] = json!(1);
                    let res = bearer(&s, Method::POST, "/v1/posts", &team)
                        .json(&body)
                        .await;
                    assert_eq!(res.status_code(), 201, "{}", res.text());
                    let made = res.json::<Value>();
                    let p4 = made["id"].as_str().unwrap().to_owned();
                    assert_eq!(made["lists"][0]["position"], 1);
                    assert_eq!(items(&s, &team, &list).await, ranked(&[&p4, &p3, &p2]));
                    let other = new_list(&s, &team, json!({"name": "Other"})).await;
                    let mut body = post_body(Some(&default), "edited");
                    body["list_id"] = json!(other);
                    let res = bearer(&s, Method::PUT, &format!("/v1/posts/{p2}"), &team)
                        .json(&body)
                        .await;
                    res.assert_status_ok();
                    let positions: Vec<(String, i64)> = res.json::<Value>()["lists"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|l| {
                            (
                                l["id"].as_str().unwrap().to_owned(),
                                l["position"].as_i64().unwrap(),
                            )
                        })
                        .collect();
                    assert_eq!(positions, vec![(list.clone(), 3), (other.clone(), 1)]);
                    // A list unknown to the post's write rolls the create back.
                    let mut body = post_body(Some(&default), "orphan");
                    body["list_id"] = json!(uuid::Uuid::now_v7());
                    let before = bearer(&s, Method::GET, "/v1/posts", &team)
                        .await
                        .json::<Vec<Value>>()
                        .len();
                    let res = bearer(&s, Method::POST, "/v1/posts", &team)
                        .json(&body)
                        .await;
                    assert_eq!(res.status_code(), 404);
                    let after = bearer(&s, Method::GET, "/v1/posts", &team)
                        .await
                        .json::<Vec<Value>>()
                        .len();
                    assert_eq!(before, after);

                    // A taken-down post drops out and a deleted draft leaves; the rest close up.
                    db.execute_raw(Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        "UPDATE posts SET status = 'deleted' WHERE id = $1",
                        [p3.parse::<uuid::Uuid>().unwrap().into()],
                    ))
                    .await
                    .unwrap();
                    assert_eq!(items(&s, &team, &list).await, ranked(&[&p4, &p2]));
                    let res = add(&s, &team, &list, json!({"post_id": p3})).await;
                    res.assert_json(&json!({"error": "invalid_post"}));
                    let res = bearer(&s, Method::DELETE, &format!("/v1/posts/{p4}"), &team).await;
                    assert_eq!(res.status_code(), 204);
                    assert_eq!(items(&s, &team, &list).await, ranked(&[&p2]));
                    assert_eq!(post_lists(&s, &team, &p2).await[0]["position"], 1);

                    // A set-tied list takes only its set's posts.
                    let tied =
                        new_list(&s, &team, json!({"name": "C", "social_set_id": clients})).await;
                    let res = add(&s, &team, &tied, json!({"post_id": p2})).await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json(&json!({"error": "invalid_set"}));
                    for set in [uuid::Uuid::now_v7().to_string(), "nope".into()] {
                        let res = bearer(&s, Method::POST, "/v1/lists", &team)
                            .json(&json!({"name": "x", "social_set_id": set}))
                            .await;
                        assert_eq!(res.status_code(), 422);
                    }

                    // A deleted list is gone; its posts stay.
                    let res = bearer(&s, Method::DELETE, &format!("/v1/lists/{list}"), &team).await;
                    assert_eq!(res.status_code(), 204);
                    let res = bearer(&s, Method::GET, &format!("/v1/lists/{list}"), &team).await;
                    assert_eq!(res.status_code(), 404);
                    let res = add(&s, &team, &list, json!({"post_id": p2})).await;
                    assert_eq!(res.status_code(), 404);
                    let all = bearer(&s, Method::GET, "/v1/lists", &team)
                        .await
                        .json::<Value>();
                    assert_eq!(ids(&all), vec![other.clone(), tied]);
                    let left = post_lists(&s, &team, &p2).await;
                    assert_eq!(left, json!([{"id": other, "name": "Other", "position": 1}]));

                    // Token writes take an Idempotency-Key.
                    let p5 = new_post(&s, &team, &default).await;
                    for replayed in [false, true] {
                        let res =
                            bearer(&s, Method::POST, &format!("/v1/lists/{other}/posts"), &team)
                                .add_header("idempotency-key", "add-p5")
                                .json(&json!({"post_id": p5}))
                                .await;
                        res.assert_status_ok();
                        assert_eq!(res.maybe_header("idempotent-replayed").is_some(), replayed);
                    }

                    let doc = s.get("/openapi.json").await.json::<Value>();
                    for (p, m) in [
                        ("/lists", "get"),
                        ("/lists", "post"),
                        ("/lists/{id}", "get"),
                        ("/lists/{id}", "patch"),
                        ("/lists/{id}", "delete"),
                        ("/lists/{id}/posts", "post"),
                        ("/lists/{id}/posts/{post_id}", "delete"),
                        ("/lists/{id}/order", "put"),
                    ] {
                        let sec = doc["paths"][p][m]["security"].to_string();
                        assert!(sec.contains("api_token"), "{p} {m} {sec}");
                    }
                }

                #[tokio::test]
                #[serial]
                async fn set_tokens_see_team_wide_lists_but_only_their_posts() {
                    let (s, db) = on_mock(1000).await;
                    let (_, default, clients, team, set) = setup(&s, &db).await;
                    let d1 = new_post(&s, &team, &default).await;
                    let d2 = new_post(&s, &team, &default).await;
                    let c1 = new_post(&s, &set, &clients).await;
                    let c2 = new_post(&s, &set, &clients).await;
                    let wide = new_list(&s, &team, json!({"name": "Wide"})).await;
                    for p in [&d1, &c1] {
                        add(&s, &team, &wide, json!({"post_id": p}))
                            .await
                            .assert_status_ok();
                    }
                    let mine =
                        new_list(&s, &set, json!({"name": "Mine", "social_set_id": clients})).await;
                    let theirs =
                        new_list(&s, &team, json!({"name": "D", "social_set_id": default})).await;

                    let seen = bearer(&s, Method::GET, "/v1/lists", &set)
                        .await
                        .json::<Value>();
                    assert_eq!(ids(&seen), vec![wide.clone(), mine.clone()]);
                    let res = bearer(&s, Method::GET, &format!("/v1/lists/{theirs}"), &set).await;
                    assert_eq!(res.status_code(), 404);
                    let res = add(&s, &set, &theirs, json!({"post_id": c1})).await;
                    assert_eq!(res.status_code(), 404);
                    assert_eq!(items(&s, &set, &wide).await, ranked(&[&c1]));
                    assert_eq!(items(&s, &team, &wide).await, ranked(&[&d1, &c1]));

                    // A set token's list defaults to its own set; another set is refused.
                    let res = bearer(&s, Method::POST, "/v1/lists", &set)
                        .json(&json!({"name": "x"}))
                        .await;
                    assert_eq!(res.status_code(), 201, "{}", res.text());
                    assert_eq!(res.json::<Value>()["social_set_id"], clients.as_str());
                    let res = bearer(&s, Method::POST, "/v1/lists", &set)
                        .json(&json!({"name": "x", "social_set_id": default}))
                        .await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json(&json!({"error": "invalid_set"}));

                    // Another set's post cannot go in; its own can, and orders among its own.
                    let res = add(&s, &set, &wide, json!({"post_id": d2})).await;
                    res.assert_json(&json!({"error": "invalid_post"}));
                    add(&s, &set, &wide, json!({"post_id": c2}))
                        .await
                        .assert_status_ok();
                    assert_eq!(items(&s, &set, &wide).await, ranked(&[&c1, &c2]));
                    assert_eq!(items(&s, &team, &wide).await, ranked(&[&d1, &c1, &c2]));
                    let res = reorder(&s, &set, &wide, &[&c2, &c1, &d1]).await;
                    res.assert_json(&json!({"error": "invalid_order"}));
                    reorder(&s, &set, &wide, &[&c2, &c1])
                        .await
                        .assert_status_ok();
                    assert_eq!(items(&s, &team, &wide).await, ranked(&[&d1, &c2, &c1]));
                    assert_eq!(post_lists(&s, &set, &c1).await[0]["position"], 2);
                    assert_eq!(post_lists(&s, &team, &c1).await[0]["position"], 3);
                    let path = format!("/v1/lists/{wide}/posts/{d1}");
                    let res = bearer(&s, Method::DELETE, &path, &set).await;
                    assert_eq!(res.status_code(), 404);

                    // The team-wide list itself is not the set token's to change.
                    let res = bearer(&s, Method::PATCH, &format!("/v1/lists/{wide}"), &set)
                        .json(&json!({"name": "mine now"}))
                        .await;
                    assert_eq!(res.status_code(), 403);
                    let res = bearer(&s, Method::DELETE, &format!("/v1/lists/{wide}"), &set).await;
                    assert_eq!(res.status_code(), 403);
                    let res = bearer(&s, Method::PATCH, &format!("/v1/lists/{mine}"), &set)
                        .json(&json!({"name": "Renamed"}))
                        .await;
                    res.assert_status_ok();
                }

                #[tokio::test]
                #[serial]
                async fn mcp_list_tools_match_rest() {
                    let (s, db) = on_mock(1000).await;
                    let (_, default, _, team, _) = setup(&s, &db).await;
                    let (p1, p2) = (
                        new_post(&s, &team, &default).await,
                        new_post(&s, &team, &default).await,
                    );
                    let (err, made) =
                        super::mcp::call(&s, &team, "create-list", json!({"name": "Via MCP"}))
                            .await;
                    assert!(!err, "{made}");
                    let list = made["id"].as_str().unwrap().to_owned();
                    for p in [&p1, &p2] {
                        let args = json!({"id": list, "post_id": p});
                        let (err, got) =
                            super::mcp::call(&s, &team, "add-post-to-list", args).await;
                        assert!(!err, "{got}");
                    }
                    let args = json!({"id": list, "post_ids": [p2, p1]});
                    let (err, _) = super::mcp::call(&s, &team, "reorder-list", args).await;
                    assert!(!err);
                    let (_, got) =
                        super::mcp::call(&s, &team, "get-list", json!({"id": list})).await;
                    let rest = bearer(&s, Method::GET, &format!("/v1/lists/{list}"), &team).await;
                    assert_eq!(got, rest.json::<Value>());
                    assert_eq!(items(&s, &team, &list).await, ranked(&[&p2, &p1]));
                    let args = json!({"id": list, "post_ids": [p2]});
                    let (err, got) = super::mcp::call(&s, &team, "reorder-list", args).await;
                    assert!(err);
                    assert_eq!(got, json!({"error": "invalid_order"}));
                    let args = json!({"id": list, "post_id": p2});
                    let (err, got) =
                        super::mcp::call(&s, &team, "remove-post-from-list", args).await;
                    assert!(!err && got == json!({"ok": true}), "{got}");
                    let (_, all) = super::mcp::call(&s, &team, "list-lists", json!({})).await;
                    let rest = bearer(&s, Method::GET, "/v1/lists", &team).await;
                    assert_eq!(all, rest.json::<Value>());
                }
            }

            /// `Idempotency-Key` on token writes.
            mod idempotency {
                use super::*;
                use axum::{
                    http::{HeaderValue, StatusCode},
                    middleware::from_fn_with_state,
                    routing::post,
                };
                use sea_orm::{ConnectionTrait, DbBackend, Statement};
                use socialreins_brain::controllers::idempotency;
                use std::future::IntoFuture;
                use std::sync::atomic::{AtomicUsize, Ordering};

                /// A team owner, the Default set id, and a whole-team token.
                async fn team(s: &TestServer, db: &DatabaseConnection) -> (String, String, String) {
                    let (_, owner, _) = user(s, db).await;
                    let team_id = current(s, &owner).await["id"].as_str().unwrap().to_owned();
                    let set = default_set(db, &team_id).await.to_string();
                    let t = make_token(s, &owner, json!({"name": "cm"})).await;
                    (owner, set, t["secret"].as_str().unwrap().to_owned())
                }

                fn create(s: &TestServer, secret: &str, key: &str, body: &Value) -> TestRequest {
                    bearer(s, Method::POST, "/v1/posts", secret)
                        .add_header("idempotency-key", key.to_owned())
                        .json(body)
                }

                async fn posts(s: &TestServer, secret: &str) -> usize {
                    let res = bearer(s, Method::GET, "/v1/posts", secret).await;
                    res.json::<Vec<Value>>().len()
                }

                #[tokio::test]
                #[serial]
                async fn a_key_runs_once_per_team() {
                    let (s, db) = on_mock(120).await;
                    let (owner, set, cm) = team(&s, &db).await;
                    let body = post_body(Some(&set), "once");

                    let first = create(&s, &cm, "order-1", &body).await;
                    assert_eq!(first.status_code(), 201, "{}", first.text());
                    assert!(first.maybe_header("idempotent-replayed").is_none());
                    let again = create(&s, &cm, "order-1", &body).await;
                    assert_eq!(again.status_code(), 201);
                    assert_eq!(again.header("idempotent-replayed"), "true");
                    assert_eq!(again.header("content-type"), "application/json");
                    assert_eq!(again.text(), first.text());
                    assert_eq!(posts(&s, &cm).await, 1);

                    // Another token of the same team shares the key.
                    let other = make_token(&s, &owner, json!({"name": "cm2"})).await;
                    let other = other["secret"].as_str().unwrap();
                    let res = create(&s, other, "order-1", &body).await;
                    assert_eq!(res.header("idempotent-replayed"), "true");

                    // The same key with another body, method or path is refused.
                    let res = create(&s, &cm, "order-1", &post_body(Some(&set), "twice")).await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json(&json!({"error": "idempotency_key_reused"}));
                    let id = first.json::<Value>()["id"].as_str().unwrap().to_owned();
                    let res = bearer(&s, Method::PUT, &format!("/v1/posts/{id}"), &cm)
                        .add_header("idempotency-key", "order-1")
                        .json(&body)
                        .await;
                    res.assert_json(&json!({"error": "idempotency_key_reused"}));

                    // A new key runs again.
                    let res = create(&s, &cm, "order-2", &body).await;
                    assert_eq!(res.status_code(), 201);
                    assert!(res.maybe_header("idempotent-replayed").is_none());
                    assert_eq!(posts(&s, &cm).await, 2);

                    // 4xx answers are kept too.
                    let bad = post_body(None, "no set");
                    let res = create(&s, &cm, "order-3", &bad).await;
                    res.assert_json(&json!({"error": "invalid_set"}));
                    let res = create(&s, &cm, "order-3", &bad).await;
                    res.assert_json(&json!({"error": "invalid_set"}));
                    assert_eq!(res.header("idempotent-replayed"), "true");

                    // Another team has its own keys.
                    let (_, theirs, them) = team(&s, &db).await;
                    let res = create(&s, &them, "order-1", &post_body(Some(&theirs), "once")).await;
                    assert_eq!(res.status_code(), 201, "{}", res.text());
                    assert!(res.maybe_header("idempotent-replayed").is_none());

                    // Sessions ignore the header.
                    for _ in 0..2 {
                        let res = req(&s, Method::POST, "/v1/posts", &owner)
                            .add_header("idempotency-key", "order-1")
                            .json(&body)
                            .await;
                        assert_eq!(res.status_code(), 201, "{}", res.text());
                        assert!(res.maybe_header("idempotent-replayed").is_none());
                    }
                    assert_eq!(posts(&s, &cm).await, 4);
                }

                #[tokio::test]
                #[serial]
                async fn invalid_keys_are_refused() {
                    let (s, db) = on_mock(120).await;
                    let (_, set, cm) = team(&s, &db).await;
                    let body = post_body(Some(&set), "x");
                    let long = "k".repeat(256);
                    for bad in [
                        HeaderValue::from_static(""),
                        HeaderValue::from_str(&long).unwrap(),
                        HeaderValue::from_bytes("caf\u{e9}".as_bytes()).unwrap(),
                    ] {
                        let res = bearer(&s, Method::POST, "/v1/posts", &cm)
                            .add_header("idempotency-key", bad.clone())
                            .json(&body)
                            .await;
                        assert_eq!(res.status_code(), 422, "{bad:?}");
                        res.assert_json(&json!({"error": "invalid_idempotency_key"}));
                    }
                    assert_eq!(posts(&s, &cm).await, 0);
                    let res = create(&s, &cm, &"k".repeat(255), &body).await;
                    assert_eq!(res.status_code(), 201);
                }

                #[tokio::test]
                #[serial]
                async fn concurrent_duplicates_run_once() {
                    let (s, db) = on_mock(120).await;
                    let (_, set, cm) = team(&s, &db).await;
                    let body = post_body(Some(&set), "race");
                    let (a, b) = tokio::join!(
                        create(&s, &cm, "race", &body).into_future(),
                        create(&s, &cm, "race", &body).into_future()
                    );
                    let mut codes = [a.status_code().as_u16(), b.status_code().as_u16()];
                    codes.sort();
                    assert!(codes == [201, 201] || codes == [201, 409], "{codes:?}");
                    assert_eq!(posts(&s, &cm).await, 1);
                }

                #[tokio::test]
                #[serial]
                async fn replays_count_once_against_the_rate_limit() {
                    let (s, db) = on_mock(3).await;
                    let (_, set, cm) = team(&s, &db).await;
                    let body = post_body(Some(&set), "x");
                    for _ in 0..3 {
                        let res = create(&s, &cm, "k", &body).await;
                        assert_eq!(res.status_code(), 201);
                    }
                    let res = create(&s, &cm, "k", &body).await;
                    assert_eq!(res.status_code(), 429);
                    res.assert_json(&json!({"error": "rate_limited"}));

                    // A body too large to buffer counts too.
                    let (s, db) = on_mock(2).await;
                    let (_, set, cm) = team(&s, &db).await;
                    let big = post_body(Some(&set), &"x".repeat(40 * 1024));
                    let res = create(&s, &cm, "big", &big).await;
                    assert_eq!(res.status_code(), 413);
                    let body = post_body(Some(&set), "x");
                    create(&s, &cm, "k", &body).await.assert_status_success();
                    let res = create(&s, &cm, "k2", &body).await;
                    assert_eq!(res.status_code(), 429);
                }

                #[tokio::test]
                #[serial]
                async fn set_tokens_never_share_answers() {
                    let (s, db) = on_mock(120).await;
                    let (owner, default, team_token) = team(&s, &db).await;
                    let other = req(&s, Method::POST, "/v1/social-sets", &owner)
                        .json(&json!({"name": "Clients"}))
                        .await
                        .json::<Value>()["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    let token_for = |set: String| {
                        let (s, owner) = (&s, owner.clone());
                        async move {
                            let t =
                                make_token(s, &owner, json!({"name": "set", "social_set_id": set}))
                                    .await;
                            t["secret"].as_str().unwrap().to_owned()
                        }
                    };
                    let (a, b) = (
                        token_for(default.clone()).await,
                        token_for(other.clone()).await,
                    );
                    let body = post_body(None, "same body");

                    let first = create(&s, &a, "shared", &body).await;
                    assert_eq!(first.status_code(), 201, "{}", first.text());
                    assert_eq!(first.json::<Value>()["social_set_id"], default.as_str());
                    // Same team, path, body and key, another set: refused, nothing replayed.
                    let res = create(&s, &b, "shared", &body).await;
                    assert_eq!(res.status_code(), 422, "{}", res.text());
                    res.assert_json(&json!({"error": "idempotency_key_reused"}));
                    assert!(res.maybe_header("idempotent-replayed").is_none());
                    // A whole-team token is not set A either.
                    let res = create(&s, &team_token, "shared", &body).await;
                    res.assert_json(&json!({"error": "idempotency_key_reused"}));
                    // Set A itself still replays.
                    let res = create(&s, &a, "shared", &body).await;
                    assert_eq!(res.header("idempotent-replayed"), "true");
                    let in_b = bearer(&s, Method::GET, "/v1/posts", &b).await;
                    assert!(in_b.json::<Vec<Value>>().is_empty());
                }

                /// A route that answers `status` and counts its runs, behind the middleware.
                fn counted(
                    state: &AppState,
                    status: StatusCode,
                ) -> (TestServer, std::sync::Arc<AtomicUsize>) {
                    let runs = std::sync::Arc::new(AtomicUsize::new(0));
                    let r = runs.clone();
                    let app = axum::Router::new()
                        .route(
                            "/run",
                            post(move || {
                                r.fetch_add(1, Ordering::SeqCst);
                                async move { status }
                            }),
                        )
                        .layer(from_fn_with_state(state.clone(), idempotency::layer));
                    (TestServer::new(app).unwrap(), runs)
                }

                #[tokio::test]
                #[serial]
                async fn server_errors_and_abandoned_runs_run_again() {
                    let config = Config::from_env().expect("test config");
                    let db = socialreins_brain::connect(&config).await.unwrap();
                    let state = AppState::new(db.clone(), &config);
                    let s = TestServer::new(controllers::router(state.clone(), &config)).unwrap();
                    let (_, owner, _) = user(&s, &db).await;
                    let team_id = current(&s, &owner).await["id"].as_str().unwrap().to_owned();
                    let cm = make_token(&s, &owner, json!({"name": "cm"})).await;
                    let cm = cm["secret"].as_str().unwrap();
                    let call = |s: &TestServer, key: &str| {
                        bearer(s, Method::POST, "/run", cm)
                            .add_header("idempotency-key", key.to_owned())
                    };

                    // A 5xx is not stored.
                    let (boom, runs) = counted(&state, StatusCode::INTERNAL_SERVER_ERROR);
                    for _ in 0..2 {
                        assert_eq!(call(&boom, "boom").await.status_code(), 500);
                    }
                    assert_eq!(runs.load(Ordering::SeqCst), 2);

                    // A run in progress answers 409; one left for over 10 minutes runs again.
                    let (ok, runs) = counted(&state, StatusCode::OK);
                    let fingerprint = token::digest(b"POST\n/run\n");
                    for (key, age) in [("fresh", 0), ("stale", 11)] {
                        let stmt = Statement::from_sql_and_values(
                            DbBackend::Postgres,
                            "INSERT INTO idempotency_keys (team_id, key, method, path, \
                             fingerprint, created_at) \
                             VALUES ($1, $2, 'POST', '/run', $3, now() - make_interval(mins => $4))",
                            [
                                team_id.parse::<uuid::Uuid>().unwrap().into(),
                                key.into(),
                                fingerprint.clone().into(),
                                age.into(),
                            ],
                        );
                        db.execute_raw(stmt).await.unwrap();
                    }
                    let res = call(&ok, "fresh").await;
                    assert_eq!(res.status_code(), 409);
                    res.assert_json(&json!({"error": "idempotency_in_progress"}));
                    assert_eq!(runs.load(Ordering::SeqCst), 0);
                    assert_eq!(call(&ok, "stale").await.status_code(), 200);
                    let res = call(&ok, "stale").await;
                    assert_eq!(res.header("idempotent-replayed"), "true");
                    assert_eq!(runs.load(Ordering::SeqCst), 1);
                }
            }
        }

        mod webhooks_api {
            use super::*;
            use axum::http::HeaderMap;
            use sea_orm::{ConnectionTrait, DbBackend, Statement};
            use socialreins_brain::webhooks;

            /// Every request the receiver took: (path, headers, body).
            static HITS: std::sync::Mutex<Vec<(String, HeaderMap, String)>> =
                std::sync::Mutex::new(Vec::new());

            /// A receiver on 127.0.0.1: `/fail` answers 500, anything else 204.
            async fn receiver() -> String {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let base = format!("http://{}", listener.local_addr().unwrap());
                let hit = |uri: axum::http::Uri, headers: HeaderMap, body: String| async move {
                    let path = uri.path().to_owned();
                    HITS.lock().unwrap().push((path.clone(), headers, body));
                    if path == "/fail" {
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR
                    } else {
                        axum::http::StatusCode::NO_CONTENT
                    }
                };
                let app = Router::new().fallback(hit);
                tokio::spawn(async move { axum::serve(listener, app).await });
                base
            }

            /// A brain on the Bluesky mock whose webhooks may reach 127.0.0.1 (tests only).
            async fn on_mock() -> (TestServer, DatabaseConnection, AppState) {
                let base = mock_bluesky().await;
                let mut config = Config::from_env().expect("test config");
                config.trust_proxy = true;
                config.bluesky.appview = base.clone();
                config.bluesky.plc = base;
                config.bluesky.allow_insecure_pds = true;
                let db = socialreins_brain::connect(&config)
                    .await
                    .expect("database reachable");
                let mut state = AppState::new(db.clone(), &config);
                state.webhooks.allow_private = true;
                let app = controllers::router(state.clone(), &config);
                (TestServer::new(app).unwrap(), db, state)
            }

            async fn put(s: &TestServer, cookie: &str, url: &str) -> axum_test::TestResponse {
                req(s, Method::PUT, "/v1/team/webhook", cookie)
                    .json(&json!({ "url": url }))
                    .await
            }

            /// (event, payload, attempts, delivered, last_status, last_error) of a team's
            /// deliveries, oldest first.
            async fn deliveries(
                db: &DatabaseConnection,
                team_id: &str,
            ) -> Vec<(String, Value, i32, bool, Option<i32>, Option<String>)> {
                let stmt = Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT event, payload, attempts, delivered_at IS NOT NULL AS done, \
                     last_status, last_error FROM webhook_deliveries \
                     WHERE team_id = $1 ORDER BY created_at, event",
                    [team_id.parse::<uuid::Uuid>().unwrap().into()],
                );
                let rows = db.query_all_raw(stmt).await.unwrap();
                rows.iter()
                    .map(|r| {
                        (
                            r.try_get("", "event").unwrap(),
                            r.try_get("", "payload").unwrap(),
                            r.try_get("", "attempts").unwrap(),
                            r.try_get("", "done").unwrap(),
                            r.try_get("", "last_status").unwrap(),
                            r.try_get("", "last_error").unwrap(),
                        )
                    })
                    .collect()
            }

            async fn exec(db: &DatabaseConnection, sql: &str, team_id: &str) {
                let stmt = Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    sql,
                    [team_id.parse::<uuid::Uuid>().unwrap().into()],
                );
                db.execute_raw(stmt).await.unwrap();
            }

            #[tokio::test]
            #[serial]
            async fn managers_set_the_webhook_and_see_the_secret_once() {
                let (s, db, _) = on_mock().await;
                let mut strict = Config::from_env().expect("test config");
                strict.trust_proxy = true;
                let prod = TestServer::new(controllers::router(
                    AppState::new(db.clone(), &strict),
                    &strict,
                ))
                .unwrap();
                let (_, owner, _) = user(&s, &db).await;
                let team_id = current(&s, &owner).await["id"].as_str().unwrap().to_owned();
                let (member, _) = joined(&s, &db, &team_id, "member").await;

                let res = s.get("/v1/team/webhook").add_header("cookie", &owner).await;
                assert_eq!(res.status_code(), 404);

                // Production refuses plain http, private literals and localhost.
                for bad in [
                    "http://example.com/hook",
                    "https://10.0.0.1/hook",
                    "https://127.0.0.1/hook",
                    "https://[::1]/hook",
                    "https://localhost/hook",
                    "https://user:pw@example.com/hook",
                    "not a url",
                ] {
                    let res = put(&prod, &owner, bad).await;
                    assert_eq!(res.status_code(), 422, "{bad}");
                    res.assert_json(&json!({"error": "invalid_url"}));
                }

                let res = put(&prod, &owner, "https://hooks.example.com/a").await;
                assert_eq!(res.status_code(), 201, "{}", res.text());
                let made = res.json::<Value>();
                let secret = made["secret"].as_str().unwrap().to_owned();
                assert!(
                    secret.starts_with("whsec_") && secret.len() == 54,
                    "{secret}"
                );
                assert_eq!(made["webhook"]["url"], "https://hooks.example.com/a");

                let res = s.get("/v1/team/webhook").add_header("cookie", &owner).await;
                res.assert_status_ok();
                assert_eq!(res.json::<Value>()["url"], "https://hooks.example.com/a");
                assert!(!res.text().contains("secret") && !res.text().contains(&secret[6..]));

                // A new URL keeps the secret and does not show it.
                let res = put(&prod, &owner, "https://hooks.example.com/b").await;
                assert_eq!(res.status_code(), 200);
                assert!(!res.text().contains("secret"));
                let stmt = Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT secret FROM team_webhooks WHERE team_id = $1",
                    [team_id.parse::<uuid::Uuid>().unwrap().into()],
                );
                let sealed: Vec<u8> = db
                    .query_one_raw(stmt)
                    .await
                    .unwrap()
                    .unwrap()
                    .try_get("", "secret")
                    .unwrap();
                assert_eq!(cipher().decrypt(&sealed).unwrap(), secret.as_bytes());

                // Members may not touch it.
                let res = s
                    .get("/v1/team/webhook")
                    .add_header("cookie", &member)
                    .await;
                assert_eq!(res.status_code(), 403);
                for (m, path) in [
                    (Method::PUT, "/v1/team/webhook"),
                    (Method::DELETE, "/v1/team/webhook"),
                    (Method::POST, "/v1/team/webhook/test"),
                ] {
                    let res = req(&s, m, path, &member)
                        .json(&json!({"url": "https://example.com"}))
                        .await;
                    assert_eq!(res.status_code(), 403, "{path}");
                    res.assert_json(&json!({"error": "forbidden"}));
                }

                // API tokens get the session-only answer.
                let token = req(&s, Method::POST, "/v1/api-tokens", &owner)
                    .json(&json!({"name": "t"}))
                    .await
                    .json::<Value>()["secret"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let res = s
                    .get("/v1/team/webhook")
                    .add_header("authorization", format!("Bearer {token}"))
                    .await;
                assert_eq!(res.status_code(), 401);
                let res = s
                    .put("/v1/team/webhook")
                    .add_header("origin", WEB)
                    .add_header("authorization", format!("Bearer {token}"))
                    .json(&json!({"url": "https://example.com"}))
                    .await;
                assert_eq!(res.status_code(), 401);

                // Setting it needs a fresh sign-in.
                let (admin, _) = joined(&s, &db, &team_id, "admin").await;
                let row = sessions::ActiveModel {
                    token_hash: Set(token::hash(admin.trim_start_matches("sr_session="))),
                    created_at: Set((Utc::now() - Duration::minutes(11)).fixed_offset()),
                    ..Default::default()
                };
                sessions::Entity::update(row).exec(&db).await.unwrap();
                let res = put(&s, &admin, "https://example.com/c").await;
                assert_eq!(res.status_code(), 403);
                res.assert_json(&json!({"error": "reauth_required"}));
                let res = s.get("/v1/team/webhook").add_header("cookie", &admin).await;
                res.assert_status_ok();

                let res = s
                    .delete("/v1/team/webhook")
                    .add_header("cookie", &owner)
                    .await;
                res.assert_json(&json!({"error": "forbidden_origin"}));
                let res = req(&s, Method::DELETE, "/v1/team/webhook", &admin).await;
                assert_eq!(res.status_code(), 204);
                let res = req(&s, Method::DELETE, "/v1/team/webhook", &owner).await;
                assert_eq!(res.status_code(), 404);
                let res = req(&s, Method::POST, "/v1/team/webhook/test", &owner).await;
                assert_eq!(res.status_code(), 404);
            }

            #[tokio::test]
            #[serial]
            async fn target_events_fire_once_signed_and_retry() {
                let (s, db, state) = on_mock().await;
                let hook = receiver().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let new_post = |body: &str| {
                    let body = json!({"social_set_id": set, "platform": "bluesky",
                        "body": body, "media": [], "scheduled_at": null});
                    let (s, cookie) = (&s, cookie.clone());
                    async move {
                        let res = req(s, Method::POST, "/v1/posts", &cookie).json(&body).await;
                        res.json::<Value>()["id"].as_str().unwrap().to_owned()
                    }
                };
                let publish_now = |id: String| {
                    let (s, cookie) = (&s, cookie.clone());
                    async move {
                        let path = format!("/v1/posts/{id}/publish");
                        assert_eq!(
                            req(s, Method::POST, &path, &cookie).await.status_code(),
                            202
                        );
                        for _ in 0..100 {
                            let p = s
                                .get(&format!("/v1/posts/{id}"))
                                .add_header("cookie", &cookie)
                                .await
                                .json::<Value>();
                            if p["status"] != "queued" {
                                return p;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        }
                        panic!("post {id} stayed queued");
                    }
                };

                // No webhook: nothing is queued.
                let quiet = new_post("before the webhook").await;
                publish_now(quiet).await;
                assert!(deliveries(&db, &team_id).await.is_empty());

                let res = put(&s, &cookie, &format!("{hook}/hook")).await;
                assert_eq!(res.status_code(), 201);
                let secret = res.json::<Value>()["secret"].as_str().unwrap().to_owned();

                let ok = new_post("hello").await;
                let p = publish_now(ok.clone()).await;
                assert_eq!(p["status"], "published");
                let target = p["targets"][0]["id"].as_str().unwrap().to_owned();
                let bad = new_post("REJECT me").await;
                assert_eq!(publish_now(bad.clone()).await["status"], "failed");
                let path = format!("/v1/posts/{ok}");
                req(&s, Method::DELETE, &path, &cookie)
                    .await
                    .assert_status_ok();
                req(&s, Method::DELETE, &path, &cookie)
                    .await
                    .assert_status_ok();
                // The same change again (a second recovery, say) queues nothing new.
                let again = Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    webhooks::with_event("UPDATE post_targets SET status = status WHERE id = $1"),
                    [target.parse::<uuid::Uuid>().unwrap().into()],
                );
                db.execute_raw(again).await.unwrap();

                let mut got = deliveries(&db, &team_id).await;
                got.sort_by(|a, b| a.0.cmp(&b.0));
                let events: Vec<&str> = got.iter().map(|d| d.0.as_str()).collect();
                assert_eq!(
                    events,
                    [
                        "post.target.deleted",
                        "post.target.failed",
                        "post.target.published"
                    ]
                );
                let published = &got[2].1;
                assert_eq!(published["type"], "post.target.published");
                let data = &published["data"];
                assert_eq!(data["post_id"], ok.as_str());
                assert_eq!(data["target_id"], target.as_str());
                assert_eq!(data["group_id"], Value::Null);
                assert_eq!(data["platform"], "bluesky");
                assert_eq!(data["status"], "published");
                assert!(data["remote_url"]
                    .as_str()
                    .unwrap()
                    .starts_with("https://bsky.app/"));
                assert!(data["social_account_id"].is_string());
                assert_eq!(got[1].1["data"]["post_id"], bad.as_str());
                assert!(got[1].1["data"]["error"].is_string());

                // The tick sends each once, signed.
                HITS.lock().unwrap().clear();
                webhooks::tick(&state).await.unwrap();
                let hits = HITS.lock().unwrap().clone();
                assert_eq!(hits.len(), 3);
                for (path, headers, body) in &hits {
                    assert_eq!(path, "/hook");
                    let sig = headers["socialreins-signature"].to_str().unwrap();
                    let t: i64 = sig[2..sig.find(',').unwrap()].parse().unwrap();
                    assert!((Utc::now().timestamp() - t).abs() < 60);
                    assert_eq!(sig, socialreins_core::webhook::signature(&secret, t, body));
                    let payload: Value = serde_json::from_str(body).unwrap();
                    assert_eq!(
                        headers["socialreins-event"],
                        payload["type"].as_str().unwrap()
                    );
                    assert_eq!(
                        headers["socialreins-delivery"],
                        payload["id"].as_str().unwrap()
                    );
                    assert_eq!(headers["content-type"], "application/json");
                }
                assert!(deliveries(&db, &team_id)
                    .await
                    .iter()
                    .all(|d| d.3 && d.2 == 1));
                webhooks::tick(&state).await.unwrap();
                assert_eq!(
                    HITS.lock().unwrap().len(),
                    3,
                    "delivered rows are not resent"
                );

                // Ping.
                let res = req(&s, Method::POST, "/v1/team/webhook/test", &cookie).await;
                res.assert_status_ok();
                res.assert_json(&json!({"status": 204, "error": null}));
                let last = HITS.lock().unwrap().last().cloned().unwrap();
                assert_eq!(last.1["socialreins-event"], "ping");

                // A failing receiver: retried after a minute, then given up after 8 tries.
                put(&s, &cookie, &format!("{hook}/fail"))
                    .await
                    .assert_status_ok();
                exec(
                    &db,
                    "UPDATE webhook_deliveries SET delivered_at = NULL, attempts = 0 \
                     WHERE team_id = $1 AND event = 'post.target.failed'",
                    &team_id,
                )
                .await;
                webhooks::tick(&state).await.unwrap();
                let d = deliveries(&db, &team_id).await;
                let failed = d.iter().find(|d| d.0 == "post.target.failed").unwrap();
                assert_eq!((failed.2, failed.3, failed.4), (1, false, Some(500)));
                assert_eq!(failed.5.as_deref(), Some("http_status"));
                let stmt = Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT (next_attempt_at - now()) BETWEEN interval '50 seconds' \
                     AND interval '61 seconds' AS ok FROM webhook_deliveries \
                     WHERE team_id = $1 AND event = 'post.target.failed'",
                    [team_id.parse::<uuid::Uuid>().unwrap().into()],
                );
                let ok_wait: bool = db
                    .query_one_raw(stmt)
                    .await
                    .unwrap()
                    .unwrap()
                    .try_get("", "ok")
                    .unwrap();
                assert!(ok_wait, "first retry waits a minute");
                let before = HITS.lock().unwrap().len();
                webhooks::tick(&state).await.unwrap();
                assert_eq!(HITS.lock().unwrap().len(), before, "not due yet");
                exec(
                    &db,
                    "UPDATE webhook_deliveries SET attempts = 7, next_attempt_at = now() \
                     WHERE team_id = $1 AND event = 'post.target.failed'",
                    &team_id,
                )
                .await;
                webhooks::tick(&state).await.unwrap();
                exec(
                    &db,
                    "UPDATE webhook_deliveries SET next_attempt_at = now() WHERE team_id = $1",
                    &team_id,
                )
                .await;
                webhooks::tick(&state).await.unwrap();
                assert_eq!(HITS.lock().unwrap().len(), before + 1, "gave up after 8");
                let d = deliveries(&db, &team_id).await;
                let failed = d.iter().find(|d| d.0 == "post.target.failed").unwrap();
                assert_eq!((failed.2, failed.3, failed.4), (8, false, Some(500)));

                // Old finished rows are purged; deleting the webhook drops the rest.
                exec(
                    &db,
                    "UPDATE webhook_deliveries SET created_at = now() - interval '31 days' \
                     WHERE team_id = $1 AND event <> 'post.target.deleted'",
                    &team_id,
                )
                .await;
                exec(
                    &db,
                    "UPDATE webhook_deliveries SET delivered_at = NULL \
                     WHERE team_id = $1 AND event = 'post.target.deleted'",
                    &team_id,
                )
                .await;
                db.execute_unprepared(&webhooks::purge_sql()).await.unwrap();
                let left: Vec<String> = deliveries(&db, &team_id)
                    .await
                    .into_iter()
                    .map(|d| d.0)
                    .collect();
                assert_eq!(left, ["post.target.deleted"]);
                let res = req(&s, Method::DELETE, "/v1/team/webhook", &cookie).await;
                assert_eq!(res.status_code(), 204);
                assert!(deliveries(&db, &team_id).await.is_empty());
            }
        }
    }
}
