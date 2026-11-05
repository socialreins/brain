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
    pub(super) async fn link(db: &DatabaseConnection, email: &str, valid_for: Duration) -> String {
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
    pub(super) async fn open(s: &TestServer, secret: &str) -> (String, Option<String>) {
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

    pub(super) fn session(set_cookie: &str) -> String {
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
                .assert_json(&json!({"google": false, "turnstile_site_key": null}));
            assert_eq!(s.get("/auth/google/start").await.status_code(), 404);
            assert_eq!(s.get("/auth/google/callback").await.status_code(), 404);
        }

        #[tokio::test]
        #[serial]
        async fn a_new_google_account_signs_up_and_lands_on_welcome() {
            let (s, db) = google_server(|_| {}).await;
            s.get("/auth/providers")
                .await
                .assert_json(&json!({"google": true, "turnstile_site_key": null}));
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
    async fn patch_me_renames_the_user() {
        let (s, db) = server_where(|_| {}).await;
        let (_, cookie, _) = user(&s, &db).await;
        let rename = |name: Value| {
            req(&s, Method::PATCH, "/auth/me", &cookie).json(&json!({ "name": name }))
        };

        let res = rename(json!("  Ada Lovelace  ")).await;
        res.assert_status_ok();
        let body = res.json::<Value>();
        assert_eq!(body["name"], "Ada Lovelace");
        let me = s.get("/auth/me").add_header("cookie", &cookie).await;
        assert_eq!(me.json::<Value>(), body);

        for bad in [json!("   "), json!("a".repeat(81))] {
            let res = rename(bad).await;
            assert_eq!(res.status_code(), 422);
            assert_eq!(res.json::<Value>(), json!({"error": "invalid_name"}));
        }
        rename(json!("é".repeat(80))).await.assert_status_ok();

        let res = s
            .patch("/auth/me")
            .add_header("origin", WEB)
            .json(&json!({"name": "x"}))
            .await;
        assert_eq!(res.status_code(), 401);
        assert_eq!(res.json::<Value>(), json!({"error": "unauthorized"}));
        let res = s
            .patch("/auth/me")
            .add_header("cookie", &cookie)
            .json(&json!({"name": "x"}))
            .await;
        assert_eq!(res.json::<Value>(), json!({"error": "forbidden_origin"}));

        let doc = s.get("/openapi.json").await.json::<Value>();
        assert!(doc["paths"]["/auth/me"]["patch"].is_object());
        assert_eq!(doc["paths"]["/auth/me"]["servers"][0]["url"], "/");
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
                .route(
                    "/slow.png",
                    get(|| async {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        image("image/png", 64)
                    }),
                )
                .route("/big.png", get(|| async { image("image/png", 1_000_001) }))
                .route("/page.html", get(|| async { image("text/html", 64) }))
                .route("/doc.pdf", get(|| async { image("application/pdf", 64) }))
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
        /// Set: the next `createRecord` is refused as an expired session.
        static EXPIRE_NEXT: std::sync::atomic::AtomicBool =
            std::sync::atomic::AtomicBool::new(false);
        /// The record key of every `deleteRecord` the mock took, newest last.
        static DELETED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

        /// `len` bytes, a real PNG signature first when the type says PNG.
        fn image(content_type: &'static str, len: usize) -> axum::response::Response {
            use axum::response::IntoResponse;
            let mut bytes = vec![7u8; len];
            if content_type == "image/png" {
                bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
            } else if content_type == "application/pdf" {
                bytes[..5].copy_from_slice(b"%PDF-");
            }
            ([("content-type", content_type)], bytes).into_response()
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
            if EXPIRE_NEXT.swap(false, std::sync::atomic::Ordering::SeqCst) {
                let err = json!({"error": "ExpiredToken"});
                return (axum::http::StatusCode::BAD_REQUEST, Json(err));
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
            DELETED
                .lock()
                .unwrap()
                .push(b["rkey"].as_str().unwrap_or_default().to_owned());
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
                media::{self, Media},
                models::{post_targets, posts},
                publish,
            };

            /// Where the tests' brain stores media: local disk, as by default.
            fn media_dir() -> std::path::PathBuf {
                let dir = std::env::temp_dir().join("socialreins-brain-test-media");
                std::fs::create_dir_all(&dir).unwrap();
                dir
            }

            /// A brain on the mock, with its state for driving the publish pipeline directly.
            async fn on_mock() -> (TestServer, DatabaseConnection, AppState) {
                let base = mock_bluesky().await;
                let mut config = Config::from_env().expect("test config");
                config.trust_proxy = true;
                config.bluesky.appview = base.clone();
                config.bluesky.plc = base;
                config.bluesky.allow_insecure_pds = true;
                let disk = object_store::local::LocalFileSystem::new_with_prefix(media_dir())
                    .unwrap()
                    .with_automatic_cleanup(true);
                config.media = Media::new(std::sync::Arc::new(disk), true);
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

            /// Points a post's cover at the mock (http on loopback, which only tests allow).
            async fn set_cover_url(db: &DatabaseConnection, base: &str, id: &str, path: &str) {
                let row = posts::ActiveModel {
                    id: Set(id.parse().unwrap()),
                    cover: Set(Some(format!("{base}{path}"))),
                    ..Default::default()
                };
                posts::Entity::update(row).exec(db).await.unwrap();
            }

            /// A published post put back as `queued` with its targets `pending` and unsent, as if a
            /// publish started again. Their `comment_status` is left as it is.
            async fn requeue_unsent(db: &DatabaseConnection, id: &str) {
                let id: uuid::Uuid = id.parse().unwrap();
                let post = posts::ActiveModel {
                    id: Set(id),
                    status: Set("queued".into()),
                    quota_reserved: Set(true),
                    ..Default::default()
                };
                posts::Entity::update(post).exec(db).await.unwrap();
                let targets = post_targets::Entity::find()
                    .filter(post_targets::Column::PostId.eq(id))
                    .all(db)
                    .await
                    .unwrap();
                for t in targets {
                    let back = post_targets::ActiveModel {
                        id: Set(t.id),
                        status: Set("pending".into()),
                        remote_id: Set(None),
                        remote_url: Set(None),
                        ..Default::default()
                    };
                    post_targets::Entity::update(back).exec(db).await.unwrap();
                }
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
                other_platform["platform"] = json!("myspace");
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
            async fn post_groups_make_one_post_per_platform_or_nothing() {
                let (s, db, _) = on_mock().await;
                let (owner, team_id, account) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let group = |body: Value| {
                    let (s, owner) = (&s, owner.clone());
                    async move {
                        req(s, Method::POST, "/v1/post-groups", &owner)
                            .json(&body)
                            .await
                    }
                };
                let count = || {
                    let db = &db;
                    async move { posts::Entity::find().count(db).await.unwrap() }
                };

                // No platforms: the set's connected ones, with each override applied.
                let list = req(&s, Method::POST, "/v1/lists", &owner)
                    .json(&json!({"name": "Series"}))
                    .await
                    .json::<Value>()["id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let res = group(json!({"social_set_id": set, "body": "everywhere",
                    "list_id": list,
                    "overrides": {"bluesky": {"body": "on bluesky",
                        "media": ["https://a.example/1.png"]}}}))
                .await;
                assert_eq!(res.status_code(), 201, "{}", res.text());
                let made = res.json::<Value>();
                let gid = made["group_id"].as_str().unwrap().to_owned();
                let ps = made["posts"].as_array().unwrap();
                assert_eq!(ps.len(), 1);
                assert_eq!(ps[0]["platform"], "bluesky");
                assert_eq!(ps[0]["body"], "on bluesky");
                assert_eq!(ps[0]["media"], json!(["https://a.example/1.png"]));
                assert_eq!(ps[0]["group_id"], gid.as_str());
                assert_eq!(ps[0]["status"], "draft");
                assert_eq!(ps[0]["lists"][0]["position"], 1);

                // Without an override every post takes the shared body; duplicates collapse.
                let res = group(
                    json!({"social_set_id": set, "platforms": ["bluesky", "bluesky"],
                    "body": "shared"}),
                )
                .await;
                assert_eq!(res.status_code(), 201, "{}", res.text());
                let other = res.json::<Value>();
                assert_eq!(other["posts"].as_array().unwrap().len(), 1);
                assert_eq!(other["posts"][0]["body"], "shared");
                assert_ne!(other["group_id"], made["group_id"]);

                // `?group_id=` lists exactly the group's posts; single posts have none.
                let single = create(&s, &owner, input(&set, "single", None)).await;
                assert_eq!(single.json::<Value>()["group_id"], Value::Null);
                let res = s
                    .get(&format!("/v1/posts?group_id={gid}"))
                    .add_header("cookie", &owner)
                    .await;
                let listed = res.json::<Vec<Value>>();
                assert_eq!(listed.len(), 1);
                assert_eq!(listed[0], ps[0]);

                // One invalid post creates nothing and names its platform.
                let before = count().await;
                for (body, code, platform) in [
                    (
                        json!({"social_set_id": set, "platforms": ["bluesky", "myspace"],
                            "body": "hi"}),
                        "unsupported_platform",
                        "myspace",
                    ),
                    (
                        json!({"social_set_id": set, "body": "hi",
                            "overrides": {"bluesky": {"body": "a".repeat(301)}}}),
                        "too_long",
                        "bluesky",
                    ),
                ] {
                    let res = group(body).await;
                    assert_eq!(res.status_code(), 422, "{code}");
                    res.assert_json(&json!({"error": code, "platform": platform}));
                }
                // An unknown list fails the post, also named.
                let res = group(json!({"social_set_id": set, "body": "hi",
                    "list_id": uuid::Uuid::now_v7()}))
                .await;
                assert_eq!(res.status_code(), 404);
                res.assert_json(&json!({"error": "not_found", "platform": "bluesky"}));
                let res = group(json!({"social_set_id": set, "body": "hi",
                    "overrides": {"x": {"body": "elsewhere"}}}))
                .await;
                assert_eq!(res.status_code(), 422);
                res.assert_json(&json!({"error": "invalid_override"}));
                let res = group(json!({"body": "hi"})).await;
                res.assert_json(&json!({"error": "invalid_set"}));
                assert_eq!(count().await, before);

                // An account on a platform brain cannot publish to stays out of the default;
                // naming that platform still fails.
                let x = uuid::Uuid::now_v7();
                let copy = sea_orm::Statement::from_sql_and_values(
                    sea_orm::DbBackend::Postgres,
                    "INSERT INTO social_accounts (id, team_id, provider, external_id, \
                     access_token) SELECT $1, team_id, 'myspace', external_id, access_token \
                     FROM social_accounts WHERE id = $2",
                    [x.into(), account.parse::<uuid::Uuid>().unwrap().into()],
                );
                sea_orm::ConnectionTrait::execute_raw(&db, copy)
                    .await
                    .unwrap();
                social_account_social_set::Entity::insert(social_account_social_set::ActiveModel {
                    social_account_id: Set(x),
                    social_set_id: Set(set.parse().unwrap()),
                })
                .exec_without_returning(&db)
                .await
                .unwrap();
                let res = group(json!({"social_set_id": set, "body": "hi"})).await;
                assert_eq!(res.status_code(), 201, "{}", res.text());
                let platforms: Vec<Value> = res.json::<Value>()["posts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| p["platform"].clone())
                    .collect();
                assert_eq!(platforms, [json!("bluesky")]);
                let res =
                    group(json!({"social_set_id": set, "platforms": ["myspace"], "body": "hi"}))
                        .await;
                res.assert_json(&json!({"error": "unsupported_platform", "platform": "myspace"}));
                assert_eq!(count().await, before + 1);

                // A group body may run well past the 4 KiB `/v1` limit.
                let pad = " ".repeat(10 * 1024);
                let big = format!(r#"{{"social_set_id":"{set}","body":"big"{pad}}}"#);
                assert!(big.len() > 10 * 1024);
                let res = req(&s, Method::POST, "/v1/post-groups", &owner)
                    .content_type("application/json")
                    .bytes(big.into())
                    .await;
                assert_eq!(res.status_code(), 201, "{}", res.text());

                // A set without accounts has no default.
                let empty = req(&s, Method::POST, "/v1/social-sets", &owner)
                    .json(&json!({"name": "Empty"}))
                    .await
                    .json::<Value>()["id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let res = group(json!({"social_set_id": empty, "body": "hi"})).await;
                res.assert_json(&json!({"error": "no_accounts"}));

                let doc = s.get("/openapi.json").await.json::<Value>();
                assert!(doc["paths"]["/post-groups"]["post"].is_object());
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
                assert_eq!(images[0]["image"]["size"], 64, "the stored bytes went up");
                assert_eq!(get_post(&s, &cookie, &not_due).await["status"], "scheduled");
                // The post settled, so its stored copy is gone, row and file.
                assert_eq!(media_rows(&db, &id).await, 0);
                assert!(!media_dir().join("posts").join(&id).exists());
            }

            #[tokio::test]
            #[serial]
            async fn slow_media_fails_the_target_once_the_deadline_passes() {
                let (s, db, mut state) = on_mock().await;
                state.media.deadline = std::time::Duration::from_secs(1);
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let base = pds_of(&db, &team_id).await;
                let id = id_of(&create(&s, &cookie, input(&set, "slow", Some(&past()))).await);
                // Five slow files: past the 4 at a time, and each within its own timeout.
                let slow = [
                    "/img.png",
                    "/slow.png",
                    "/slow.png?2",
                    "/slow.png?3",
                    "/slow.png?4",
                ];
                // One sends a PNG start then stalls, so its upload is underway at the deadline.
                let stall = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let stall_url = format!("http://{}/stall.png", stall.local_addr().unwrap());
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    while let Ok((mut c, _)) = stall.accept().await {
                        tokio::spawn(async move {
                            let head = "HTTP/1.1 200 OK\r\ncontent-type: image/png\r\n\
                                        content-length: 1000\r\n\r\n";
                            let _ = c.write_all(head.as_bytes()).await;
                            let _ = c.write_all(b"\x89PNG\r\n\x1a\n\0\0\0\0\0\0").await;
                            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                        });
                    }
                });
                let mut urls: Vec<String> =
                    slow[..3].iter().map(|p| format!("{base}{p}")).collect();
                urls.push(stall_url);
                let row = posts::ActiveModel {
                    id: Set(id.parse().unwrap()),
                    media: Set(json!(urls)),
                    thread: Set(json!([{"body": "two", "media": [format!("{base}{}", slow[4])]}])),
                    ..Default::default()
                };
                posts::Entity::update(row).exec(&db).await.unwrap();
                let started = std::time::Instant::now();
                publish::tick(&state).await.unwrap();
                assert!(
                    started.elapsed() < std::time::Duration::from_secs(4),
                    "the deadline held"
                );
                let p = get_post(&s, &cookie, &id).await;
                assert_eq!(p["status"], "failed", "{p}");
                let msg = p["targets"][0]["error"].as_str().unwrap();
                assert!(msg.starts_with("Image 2 did not download in time"), "{msg}");
                assert!(p["targets"][0]["remote_id"].is_null(), "nothing went out");
                // The aborted upload left nothing, staged or whole, and the stored image went.
                let dir = media_dir().join("posts").join(&id);
                let left: Vec<_> = std::fs::read_dir(&dir)
                    .map(|d| d.flatten().map(|e| e.file_name()).collect())
                    .unwrap_or_default();
                assert!(left.is_empty(), "{left:?}");
            }

            async fn media_rows(db: &DatabaseConnection, post: &str) -> i64 {
                use sea_orm::{ConnectionTrait, DbBackend, Statement};
                let stmt = Statement::from_sql_and_values(
                    DbBackend::Postgres,
                    "SELECT count(*) AS n FROM post_media WHERE post_id = $1",
                    vec![post.parse::<uuid::Uuid>().unwrap().into()],
                );
                let row = db.query_one_raw(stmt).await.unwrap().unwrap();
                row.try_get("", "n").unwrap()
            }

            #[tokio::test]
            #[serial]
            async fn media_is_stored_once_and_swept_when_no_post_needs_it() {
                use sea_orm::{ConnectionTrait, DbBackend, Statement};
                let (s, db, state) = on_mock().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let base = pds_of(&db, &team_id).await;
                let url = format!("{base}/img.png");
                let mut ids = Vec::new();
                for at in [Some(past()), None, Some(past())] {
                    let later = at.map(|_| (Utc::now() + Duration::hours(1)).to_rfc3339());
                    ids.push(id_of(
                        &create(&s, &cookie, input(&set, "kept", later.as_deref())).await,
                    ));
                }
                let (done, draft, fresh) = (&ids[0], &ids[1], &ids[2]);
                let soon = || tokio::time::Instant::now() + media::INGEST_DEADLINE;
                let ingest = |id: &str| {
                    let (media, db, url) = (state.media.clone(), db.clone(), url.clone());
                    let id: uuid::Uuid = id.parse().unwrap();
                    async move {
                        media
                            .ingest(&db, id, &url, 1_000_000, soon())
                            .await
                            .unwrap()
                    }
                };
                // A retry reuses the stored copy: one row, one key.
                let a = ingest(done).await;
                assert_eq!(ingest(done).await.key, a.key);
                assert_eq!(a.content_type, "image/png");
                assert_eq!(media_rows(&db, done).await, 1);
                assert_eq!(state.media.read(&a.key).await.unwrap().len(), 64);
                // A row whose file is gone is a miss: downloaded again, under a new key.
                let path = |k: &str| object_store::path::Path::from(k);
                state.media.store.delete(&path(&a.key)).await.unwrap();
                let a = ingest(done).await;
                assert_eq!(state.media.read(&a.key).await.unwrap().len(), 64);
                assert_eq!(media_rows(&db, done).await, 1);
                ingest(draft).await;
                ingest(fresh).await;
                // Files no row names (a lost insert, a crash) go too.
                let put = |k: String| {
                    let store = state.media.store.clone();
                    async move {
                        store.put(&path(&k), vec![1u8].into()).await.unwrap();
                        k
                    }
                };
                let orphan = put(format!("posts/{done}/orphan")).await;
                let gone_post = put(format!("posts/{}/orphan", uuid::Uuid::now_v7())).await;
                let draft_orphan = put(format!("posts/{draft}/orphan")).await;
                // A week old, by the file's own time and the row's.
                let week_old = |k: &str| {
                    let f = std::fs::File::options()
                        .write(true)
                        .open(media_dir().join(k))
                        .unwrap();
                    let at =
                        std::time::SystemTime::now() - std::time::Duration::from_secs(8 * 86_400);
                    f.set_modified(at).unwrap();
                };
                for k in [&a.key, &orphan, &gone_post, &draft_orphan] {
                    week_old(k);
                }
                let age = |id: &str, days: i32| {
                    Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        "UPDATE post_media SET created_at = now() - make_interval(days => $2) \
                         WHERE post_id = $1",
                        vec![id.parse::<uuid::Uuid>().unwrap().into(), days.into()],
                    )
                };
                for (id, days) in [(done, 8), (draft, 8), (fresh, 1)] {
                    db.execute_raw(age(id, days)).await.unwrap();
                }
                for id in [done, fresh] {
                    let row = posts::ActiveModel {
                        id: Set(id.parse().unwrap()),
                        status: Set("published".into()),
                        ..Default::default()
                    };
                    posts::Entity::update(row).exec(&db).await.unwrap();
                }
                // `done` settled and `fresh` too, but only `done`'s files are a week old.
                assert_eq!(media::sweep(&state).await.unwrap(), 3);
                assert_eq!(media_rows(&db, done).await, 0);
                for k in [&a.key, &orphan, &gone_post] {
                    assert!(state.media.read(k).await.is_err(), "{k} is gone");
                }
                assert!(
                    state.media.read(&draft_orphan).await.is_ok(),
                    "a draft keeps its files"
                );
                assert_eq!(media_rows(&db, draft).await, 1, "a draft still needs it");
                assert_eq!(media_rows(&db, fresh).await, 1, "not a week old yet");
                // Once a post settles, everything under its prefix goes, named by a row or not.
                let fresh_orphan = put(format!("posts/{fresh}/orphan")).await;
                state.media.release_post(&db, fresh.parse().unwrap()).await;
                assert_eq!(media_rows(&db, fresh).await, 0);
                assert!(state.media.read(&fresh_orphan).await.is_err());
                assert!(!media_dir().join("posts").join(fresh).exists());
                // Deleting the draft deletes its files right away.
                let res = req(&s, Method::DELETE, &format!("/v1/posts/{draft}"), &cookie).await;
                assert_eq!(res.status_code(), 204);
                assert_eq!(media_rows(&db, draft).await, 0);
                assert!(state.media.read(&draft_orphan).await.is_err());
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
                    (
                        "pdf",
                        vec!["/doc.pdf"],
                        "Image 1 is a PDF. Only LinkedIn posts take a PDF, as a document.",
                    ),
                    (
                        "missing",
                        vec!["/img.png", "/no/such.png"],
                        "Image 2 could not be downloaded (HTTP 404).",
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

            fn thread_input(set: &str, body: &str, parts: &[&str]) -> Value {
                let mut v = input(set, body, Some(&past()));
                v["thread"] = parts.iter().map(|p| json!({"body": p})).collect();
                v
            }

            fn rkey(uri: &str) -> String {
                uri.rsplit('/').next().unwrap().to_owned()
            }

            #[tokio::test]
            #[serial]
            async fn threads_are_validated_and_round_trip() {
                let (s, db, _) = on_mock().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let long = "a".repeat(301);
                let mut five_images = thread_input(&set, "root", &["ok", "ok"]);
                five_images["thread"][1]["media"] = json!(vec!["https://a.example/1.png"; 5]);
                let many: Vec<&str> = vec!["x"; 26];
                for (body, want) in [
                    (
                        thread_input(&set, "root", &many),
                        json!({"error": "too_many_parts"}),
                    ),
                    (
                        thread_input(&set, "root", &["ok", &long]),
                        json!({"error": "too_long", "part": 2}),
                    ),
                    (five_images, json!({"error": "too_many_images", "part": 2})),
                    (
                        thread_input(&set, "root", &[" "]),
                        json!({"error": "empty_post", "part": 1}),
                    ),
                ] {
                    let res = create(&s, &cookie, body).await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json(&want);
                }
                let mut body = input(&set, "root", None);
                body["thread"] = json!([{"body": "two", "media": ["https://a.example/i.png"]},
                    {"body": "three"}]);
                let res = create(&s, &cookie, body).await;
                assert_eq!(res.status_code(), 201);
                let p = res.json::<Value>();
                assert_eq!(
                    p["thread"],
                    json!([{"body": "two", "media": ["https://a.example/i.png"]},
                        {"body": "three", "media": []}])
                );
                let id = id_of_post(&p);
                assert_eq!(get_post(&s, &cookie, &id).await["thread"], p["thread"]);
                let path = format!("/v1/posts/{id}");
                let res = req(&s, Method::PUT, &path, &cookie)
                    .json(&thread_input(&set, "root", &["only"]))
                    .await;
                res.assert_status_ok();
                assert_eq!(
                    res.json::<Value>()["thread"],
                    json!([{"body": "only", "media": []}])
                );
                // A full thread fits the body limit.
                let full = "🦀".repeat(300);
                let parts: Vec<&str> = vec![&full; 25];
                let res = req(&s, Method::PUT, &path, &cookie)
                    .json(&thread_input(&set, "root", &parts))
                    .await;
                res.assert_status_ok();
                let res = req(&s, Method::PUT, &path, &cookie)
                    .json(&input(&set, "no thread", None))
                    .await;
                assert_eq!(res.json::<Value>()["thread"], json!([]));
            }

            fn id_of_post(p: &Value) -> String {
                p["id"].as_str().unwrap().to_owned()
            }

            #[tokio::test]
            #[serial]
            async fn a_thread_goes_out_as_replies_and_comes_down_children_first() {
                let (s, db, state) = on_mock().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let id =
                    id_of(&create(&s, &cookie, thread_input(&set, "one", &["two", "three"])).await);
                let before = RECORDS.lock().unwrap().len();
                publish::tick(&state).await.unwrap();
                let p = get_post(&s, &cookie, &id).await;
                assert_eq!(p["status"], "published", "{p}");
                let t = &p["targets"][0];
                assert_eq!(t["status"], "published");
                let root = t["remote_id"].as_str().unwrap().to_owned();
                let parts = t["parts"].as_array().unwrap();
                assert_eq!(parts.len(), 2);
                assert!(parts[1]["remote_url"]
                    .as_str()
                    .unwrap()
                    .starts_with("https://bsky.app/profile/"));
                let records: Vec<Value> = RECORDS.lock().unwrap()[before..].to_vec();
                let texts: Vec<&str> = records
                    .iter()
                    .map(|r| r["record"]["text"].as_str().unwrap())
                    .collect();
                assert_eq!(texts, ["one", "two", "three"]);
                assert!(records[0]["record"].get("reply").is_none());
                let reply = |i: usize| records[i]["record"]["reply"].clone();
                assert_eq!(reply(1)["root"], json!({"uri": root, "cid": "bafycid"}));
                assert_eq!(reply(1)["parent"]["uri"], root.as_str());
                assert_eq!(reply(2)["root"]["uri"], root.as_str());
                assert_eq!(reply(2)["parent"]["uri"], parts[0]["remote_id"]);
                let row = post_targets::Entity::find()
                    .filter(post_targets::Column::PostId.eq(id.parse::<uuid::Uuid>().unwrap()))
                    .one(&db)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(row.remote_cid.as_deref(), Some("bafycid"));
                assert_eq!(row.parts[0]["cid"], "bafycid");

                let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                res.assert_status_ok();
                let body = res.json::<Value>();
                assert_eq!(body["status"], "deleted");
                assert_eq!(body["targets"][0]["parts"], json!([]));
                let deleted = DELETED.lock().unwrap().clone();
                let n = deleted.len();
                assert_eq!(
                    deleted[n - 3..],
                    [
                        rkey(parts[1]["remote_id"].as_str().unwrap()),
                        rkey(parts[0]["remote_id"].as_str().unwrap()),
                        rkey(&root),
                    ]
                );
            }

            #[tokio::test]
            #[serial]
            async fn an_expired_cached_session_is_opened_again() {
                let (s, db, state) = on_mock().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let body = thread_input(&set, "one", &["two"]);
                let first = id_of(&create(&s, &cookie, body).await);
                publish::tick(&state).await.unwrap();
                assert_eq!(get_post(&s, &cookie, &first).await["status"], "published");
                // The cached session now answers 400 ExpiredToken once, mid-thread.
                let id = id_of(&create(&s, &cookie, thread_input(&set, "a", &["b"])).await);
                let before = RECORDS.lock().unwrap().len();
                EXPIRE_NEXT.store(true, std::sync::atomic::Ordering::SeqCst);
                publish::tick(&state).await.unwrap();
                let p = get_post(&s, &cookie, &id).await;
                assert_eq!(p["status"], "published", "{p}");
                assert_eq!(p["targets"][0]["parts"].as_array().unwrap().len(), 1);
                assert_eq!(RECORDS.lock().unwrap().len(), before + 2, "each sent once");
                assert!(!EXPIRE_NEXT.load(std::sync::atomic::Ordering::SeqCst));
            }

            #[tokio::test]
            #[serial]
            async fn a_failed_part_keeps_the_parts_before_it() {
                let (s, db, state) = on_mock().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let body = thread_input(&set, "one", &["two", "REJECT three", "four"]);
                let id = id_of(&create(&s, &cookie, body).await);
                let before = RECORDS.lock().unwrap().len();
                publish::tick(&state).await.unwrap();
                assert_eq!(RECORDS.lock().unwrap().len(), before + 2, "four never went");
                let p = get_post(&s, &cookie, &id).await;
                // The root is up, so the post is published and can come down.
                assert_eq!(p["status"], "published", "{p}");
                let t = &p["targets"][0];
                assert_eq!(t["status"], "failed");
                assert_eq!(
                    t["error"],
                    "Thread post 2 of 3: Bluesky refused it (InvalidRequest). The posts before \
                     it are up."
                );
                assert!(p["error"].as_str().unwrap().contains("Thread post 2 of 3"));
                assert_eq!(t["parts"].as_array().unwrap().len(), 1);
                assert!(t["remote_url"].is_string());

                // Never sent again: the target stays failed with its saved part.
                publish::publish_post(&state, id.parse().unwrap())
                    .await
                    .unwrap();
                publish::recover_stuck(&db).await.unwrap();
                assert_eq!(RECORDS.lock().unwrap().len(), before + 2);

                let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                res.assert_status_ok();
                let body = res.json::<Value>();
                assert_eq!(body["status"], "deleted");
                assert_eq!(body["targets"][0]["status"], "deleted");
            }

            #[tokio::test]
            #[serial]
            async fn a_part_that_will_not_come_down_keeps_the_root_up() {
                let (s, db, state) = on_mock().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let body = thread_input(&set, "one", &["STICKY two", "three"]);
                let id = id_of(&create(&s, &cookie, body).await);
                publish::tick(&state).await.unwrap();
                let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                assert_eq!(res.status_code(), 422);
                let p = get_post(&s, &cookie, &id).await;
                assert_eq!(p["status"], "published");
                let t = &p["targets"][0];
                assert_eq!(t["status"], "published");
                let parts = t["parts"].as_array().unwrap();
                assert_eq!(parts.len(), 1, "three came down, the sticky part stays");
                assert!(parts[0]["remote_id"].as_str().unwrap().contains("sticky"));
            }

            #[tokio::test]
            #[serial]
            async fn a_thread_cut_off_midway_fails_naming_the_next_part() {
                let (s, db, _) = on_mock().await;
                let (cookie, team_id, account) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let did = list(&s, &cookie).await[0]["external_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let post = queued_post(&db, &team_id, &set).await;
                let row = posts::ActiveModel {
                    id: Set(post),
                    thread: Set(json!([{"body": "a"}, {"body": "b"}])),
                    ..Default::default()
                };
                posts::Entity::update(row).exec(&db).await.unwrap();
                let id = uuid::Uuid::now_v7();
                let row = post_targets::ActiveModel {
                    id: Set(id),
                    post_id: Set(post),
                    social_account_id: Set(Some(account.parse().unwrap())),
                    status: Set("publishing".into()),
                    remote_id: Set(Some(format!("at://{did}/app.bsky.feed.post/r3root"))),
                    parts: Set(
                        json!([{"remote_id": format!("at://{did}/app.bsky.feed.post/r3a"),
                        "cid": "c"}]),
                    ),
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
                assert_eq!(t.status, "failed");
                assert!(t.error.unwrap().contains("thread post 2 of 2"));
                let p = get_post(&s, &cookie, &post.to_string()).await;
                assert_eq!(p["status"], "published");
                let res = req(&s, Method::DELETE, &format!("/v1/posts/{post}"), &cookie).await;
                res.assert_status_ok();
                let deleted = DELETED.lock().unwrap().clone();
                assert_eq!(deleted[deleted.len() - 2..], ["r3a", "r3root"]);
            }

            #[tokio::test]
            #[serial]
            async fn post_group_overrides_carry_a_thread() {
                let (s, db, _) = on_mock().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let res = req(&s, Method::POST, "/v1/post-groups", &cookie)
                    .json(&json!({"social_set_id": set, "body": "hi",
                        "thread": [{"body": "shared"}],
                        "overrides": {"bluesky": {"thread": [{"body": "own"}, {"body": "two"}]}}}))
                    .await;
                assert_eq!(res.status_code(), 201, "{}", res.text());
                let g = res.json::<Value>();
                assert_eq!(
                    g["posts"][0]["thread"],
                    json!([{"body": "own", "media": []}, {"body": "two", "media": []}])
                );
                let long = "a".repeat(301);
                let res = req(&s, Method::POST, "/v1/post-groups", &cookie)
                    .json(&json!({"social_set_id": set, "body": "hi",
                        "thread": [{"body": long}]}))
                    .await;
                assert_eq!(res.status_code(), 422);
                res.assert_json(&json!({"error": "too_long", "part": 1, "platform": "bluesky"}));
            }

            #[tokio::test]
            #[serial]
            async fn first_comments_are_checked_per_platform_and_saved_with_the_post() {
                let (s, db, _) = on_mock().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let with = |platform: &str, comment: Value| {
                    let mut v = input(&set, "hi", None);
                    v["platform"] = json!(platform);
                    v["media"] = json!(["https://a.example/clip.mp4"]);
                    v["first_comment"] = comment;
                    v
                };
                // Only Facebook, Instagram, LinkedIn and YouTube comment.
                for p in ["bluesky", "threads", "x", "tiktok"] {
                    let res = create(&s, &cookie, with(p, json!("sources"))).await;
                    assert_eq!(res.status_code(), 422, "{p}");
                    res.assert_json(&json!({"error": "first_comment_not_supported"}));
                }
                // Their limits, in characters; blank is none.
                for (p, max) in [("facebook", 8000), ("linkedin", 1250), ("youtube", 10_000)] {
                    let ok = with(p, json!("é".repeat(max)));
                    let res = create(&s, &cookie, ok).await;
                    assert_eq!(res.status_code(), 201, "{p}: {}", res.text());
                    let long = with(p, json!("é".repeat(max + 1)));
                    let res = create(&s, &cookie, long).await;
                    assert_eq!(res.status_code(), 422, "{p}");
                    res.assert_json(&json!({"error": "first_comment_too_long"}));
                }
                let res = create(&s, &cookie, with("linkedin", json!("  \n"))).await;
                assert_eq!(res.status_code(), 201);
                assert_eq!(res.json::<Value>()["first_comment"], Value::Null);
                assert_eq!(res.json::<Value>()["targets"], json!([]));

                // Saved, replaced and cleared with the post.
                let res = create(&s, &cookie, with("linkedin", json!("first"))).await;
                assert_eq!(res.json::<Value>()["first_comment"], "first");
                let id = id_of(&res);
                let put = |body: Value| {
                    req(&s, Method::PUT, &format!("/v1/posts/{id}"), &cookie).json(&body)
                };
                let res = put(with("linkedin", json!("second"))).await;
                assert_eq!(res.json::<Value>()["first_comment"], "second");
                let res = put(with("bluesky", json!("nope"))).await;
                assert_eq!(res.status_code(), 422);
                let mut cleared = with("linkedin", Value::Null);
                cleared.as_object_mut().unwrap().remove("first_comment");
                let res = put(cleared).await;
                assert_eq!(res.json::<Value>()["first_comment"], Value::Null);
                assert_eq!(
                    get_post(&s, &cookie, &id).await["first_comment"],
                    Value::Null
                );
            }

            #[tokio::test]
            #[serial]
            async fn post_group_first_comments_take_per_platform_overrides() {
                let (s, db, _) = on_mock().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let group =
                    |body: Value| req(&s, Method::POST, "/v1/post-groups", &cookie).json(&body);
                // A platform that cannot comment refuses the shared text, naming itself.
                let res = group(json!({"social_set_id": set, "body": "hi",
                    "platforms": ["bluesky", "linkedin"], "first_comment": "sources"}))
                .await;
                assert_eq!(res.status_code(), 422);
                res.assert_json(&json!({"error": "first_comment_not_supported",
                    "platform": "bluesky"}));
                // An empty override leaves it out there; another override replaces it.
                let res = group(json!({"social_set_id": set, "body": "hi",
                    "platforms": ["bluesky", "linkedin", "facebook"],
                    "first_comment": "sources",
                    "overrides": {"bluesky": {"first_comment": ""},
                        "facebook": {"first_comment": "own"}}}))
                .await;
                assert_eq!(res.status_code(), 201, "{}", res.text());
                let g = res.json::<Value>();
                let comments: Vec<&Value> = g["posts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| &p["first_comment"])
                    .collect();
                assert_eq!(comments, [&Value::Null, &json!("sources"), &json!("own")]);
                // A bad override is that platform's error.
                let res = group(json!({"social_set_id": set, "body": "hi",
                    "platforms": ["linkedin"],
                    "overrides": {"linkedin": {"first_comment": "a".repeat(1251)}}}))
                .await;
                assert_eq!(res.status_code(), 422);
                res.assert_json(&json!({"error": "first_comment_too_long",
                    "platform": "linkedin"}));
            }

            #[tokio::test]
            #[serial]
            async fn covers_are_for_one_video_on_three_platforms_and_saved_with_the_post() {
                let (s, db, _) = on_mock().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let with = |platform: &str, media: Value, cover: Value| {
                    let mut v = input(&set, "hi", None);
                    v["platform"] = json!(platform);
                    v["media"] = media;
                    v["cover"] = cover;
                    v
                };
                let clip = json!(["https://a.example/clip.mp4"]);
                let cover = json!("https://a.example/cover.jpg");
                // TikTok only picks a frame, the rest have no cover at all.
                for p in ["bluesky", "threads", "x", "tiktok", "linkedin"] {
                    let res = create(&s, &cookie, with(p, clip.clone(), cover.clone())).await;
                    assert_eq!(res.status_code(), 422, "{p}");
                    res.assert_json(&json!({"error": "cover_not_supported"}));
                }
                for p in ["facebook", "instagram", "youtube"] {
                    let res = create(&s, &cookie, with(p, clip.clone(), cover.clone())).await;
                    assert_eq!(res.status_code(), 201, "{p}: {}", res.text());
                    assert_eq!(res.json::<Value>()["cover"], cover);
                    // A public https link.
                    let http = json!("http://a.example/cover.jpg");
                    let res = create(&s, &cookie, with(p, clip.clone(), http)).await;
                    assert_eq!(res.status_code(), 422, "{p}");
                    res.assert_json(&json!({"error": "invalid_cover"}));
                }
                // Exactly one media item (YouTube takes one at most, Instagram needs media).
                let two = json!(["https://a.example/a.mp4", "https://a.example/b.mp4"]);
                for (p, media) in [
                    ("facebook", json!([])),
                    ("facebook", two.clone()),
                    ("instagram", two),
                ] {
                    let res = create(&s, &cookie, with(p, media, cover.clone())).await;
                    assert_eq!(res.status_code(), 422, "{p}");
                    res.assert_json(&json!({"error": "cover_needs_one_video"}));
                }
                // Blank is none.
                let res = create(&s, &cookie, with("facebook", clip.clone(), json!("  "))).await;
                assert_eq!(res.status_code(), 201);
                assert_eq!(res.json::<Value>()["cover"], Value::Null);

                // Saved, replaced and cleared with the post.
                let res = create(&s, &cookie, with("facebook", clip.clone(), cover)).await;
                let id = id_of(&res);
                let put = |body: Value| {
                    req(&s, Method::PUT, &format!("/v1/posts/{id}"), &cookie).json(&body)
                };
                let other = json!("https://a.example/other.png");
                let res = put(with("facebook", clip.clone(), other.clone())).await;
                assert_eq!(res.json::<Value>()["cover"], other);
                let res = put(with("bluesky", clip.clone(), other)).await;
                assert_eq!(res.status_code(), 422);
                let mut cleared = with("facebook", clip, Value::Null);
                cleared.as_object_mut().unwrap().remove("cover");
                let res = put(cleared).await;
                assert_eq!(res.json::<Value>()["cover"], Value::Null);
                assert_eq!(get_post(&s, &cookie, &id).await["cover"], Value::Null);
            }

            #[tokio::test]
            #[serial]
            async fn post_group_covers_take_per_platform_overrides() {
                let (s, db, _) = on_mock().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                let group =
                    |body: Value| req(&s, Method::POST, "/v1/post-groups", &cookie).json(&body);
                let clip = json!(["https://a.example/clip.mp4"]);
                // A platform without a cover refuses the shared one, naming itself.
                let res = group(json!({"social_set_id": set, "body": "hi", "media": clip,
                    "platforms": ["tiktok", "youtube"], "cover": "https://a.example/c.jpg"}))
                .await;
                assert_eq!(res.status_code(), 422);
                res.assert_json(&json!({"error": "cover_not_supported", "platform": "tiktok"}));
                // An empty override leaves it out there; another replaces it.
                let res = group(json!({"social_set_id": set, "body": "hi", "media": clip,
                    "platforms": ["tiktok", "youtube", "facebook"],
                    "cover": "https://a.example/c.jpg",
                    "overrides": {"tiktok": {"cover": ""},
                        "facebook": {"cover": "https://a.example/own.png"}}}))
                .await;
                assert_eq!(res.status_code(), 201, "{}", res.text());
                let g = res.json::<Value>();
                let covers: Vec<&Value> = g["posts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| &p["cover"])
                    .collect();
                assert_eq!(
                    covers,
                    [
                        &Value::Null,
                        &json!("https://a.example/c.jpg"),
                        &json!("https://a.example/own.png")
                    ]
                );
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

            /// Threads: the connect flow, publishing through the Graph API mock, take down and
            /// the daily token refresh, plus the brain links Threads fetches media from.
            mod threads_api {
                use super::*;
                use axum::{
                    extract::{Form, Path, Query},
                    routing::{delete, get, post},
                    Json, Router,
                };
                use socialreins_brain::{connectors::threads::Threads, controllers::threads};
                use std::sync::atomic::{AtomicU64, Ordering};

                /// Every container the mock made: (user, params), oldest first.
                static CONTAINERS: std::sync::Mutex<Vec<(String, HashMap<String, String>)>> =
                    std::sync::Mutex::new(Vec::new());
                /// Every `threads_publish` creation id, and every deleted post id.
                static PUBLISHED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
                static REMOVED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
                static NEXT: AtomicU64 = AtomicU64::new(1);

                type Answer = (axum::http::StatusCode, Json<Value>);

                fn ok(v: Value) -> Answer {
                    (axum::http::StatusCode::OK, Json(v))
                }

                fn refused(code: i64, message: &str) -> Answer {
                    let body = json!({"error": {"message": message, "code": code}});
                    (axum::http::StatusCode::BAD_REQUEST, Json(body))
                }

                /// The bearer token, when it is one the mock handed out.
                fn token_ok(h: &axum::http::HeaderMap) -> bool {
                    let t = h.get("authorization").and_then(|v| v.to_str().ok());
                    matches!(t, Some("Bearer long" | "Bearer refreshed"))
                }

                /// The Graph API, plus an image and a video for posts to point at.
                async fn mock_threads() -> String {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let base = format!("http://{}", listener.local_addr().unwrap());
                    let code = |Form(f): Form<HashMap<String, String>>| async move {
                        let good = f.get("client_secret").map(String::as_str) == Some("tsecret")
                            && f.get("client_id").map(String::as_str) == Some("tapp")
                            && f.get("grant_type").map(String::as_str)
                                == Some("authorization_code")
                            && f.get("code").map(String::as_str) == Some("good");
                        if good {
                            ok(json!({"access_token": "short", "user_id": 1789}))
                        } else {
                            refused(100, "Invalid code")
                        }
                    };
                    let exchange = |Query(q): Query<HashMap<String, String>>| async move {
                        let good = q.get("grant_type").map(String::as_str)
                            == Some("th_exchange_token")
                            && q.get("client_secret").map(String::as_str) == Some("tsecret")
                            && q.get("access_token").map(String::as_str) == Some("short");
                        if good {
                            ok(json!({"access_token": "long", "token_type": "bearer",
                                "expires_in": 5_184_000}))
                        } else {
                            refused(190, "bad token")
                        }
                    };
                    let refresh = |Query(q): Query<HashMap<String, String>>| async move {
                        let good = q.get("grant_type").map(String::as_str)
                            == Some("th_refresh_token")
                            && q.get("access_token").map(String::as_str) == Some("long");
                        if good {
                            ok(json!({"access_token": "refreshed", "expires_in": 5_184_000}))
                        } else {
                            refused(190, "expired")
                        }
                    };
                    let me = |h: axum::http::HeaderMap| async move {
                        if !token_ok(&h) {
                            return refused(190, "expired");
                        }
                        ok(json!({"id": "1789", "username": "ann", "name": "Ann Lee",
                            "threads_profile_picture_url": "https://scontent.example/p.jpg"}))
                    };
                    let container =
                        |h: axum::http::HeaderMap,
                         Path(user): Path<String>,
                         Form(f): Form<HashMap<String, String>>| async move {
                            if !token_ok(&h) {
                                return refused(190, "Error validating access token");
                            }
                            CONTAINERS.lock().unwrap().push((user, f));
                            ok(json!({"id": format!("c{}", NEXT.fetch_add(1, Ordering::SeqCst))}))
                        };
                    let publish =
                        |h: axum::http::HeaderMap, Form(f): Form<HashMap<String, String>>| async move {
                            if !token_ok(&h) {
                                return refused(190, "expired");
                            }
                            let creation = f.get("creation_id").cloned().unwrap_or_default();
                            PUBLISHED.lock().unwrap().push(creation);
                            ok(json!({"id": NEXT.fetch_add(1, Ordering::SeqCst).to_string()}))
                        };
                    let node =
                        |h: axum::http::HeaderMap,
                         Path(id): Path<String>,
                         Query(q): Query<HashMap<String, String>>| async move {
                            if !token_ok(&h) {
                                return refused(190, "expired");
                            }
                            match q.get("fields").map(String::as_str) {
                                Some("status,error_message") => ok(json!({"status": "FINISHED"})),
                                Some("permalink") => ok(json!({
                                "permalink": format!("https://www.threads.com/@ann/post/P{id}")})),
                                _ => refused(100, "unknown fields"),
                            }
                        };
                    let remove = |h: axum::http::HeaderMap, Path(id): Path<String>| async move {
                        if !token_ok(&h) {
                            return refused(190, "expired");
                        }
                        REMOVED.lock().unwrap().push(id);
                        ok(json!({"success": true}))
                    };
                    let mut mp4 = vec![0u8; 64];
                    mp4[..12].copy_from_slice(b"\0\0\0\x18ftypisom");
                    let app = Router::new()
                        .route("/oauth/access_token", post(code))
                        .route("/access_token", get(exchange))
                        .route("/refresh_access_token", get(refresh))
                        .route("/v1.0/me", get(me))
                        .route("/v1.0/{user}/threads", post(container))
                        .route("/v1.0/{user}/threads_publish", post(publish))
                        .route("/v1.0/{id}", get(node).merge(delete(remove)))
                        .route("/img.png", get(|| async { image("image/png", 64) }))
                        .route(
                            "/clip.mp4",
                            get(move || async move { ([("content-type", "video/mp4")], mp4) }),
                        );
                    tokio::spawn(async move { axum::serve(listener, app).await });
                    base
                }

                /// A brain with Threads set up on the mock (no waits), its state, and the mock.
                async fn on_threads(
                    configured: bool,
                ) -> (TestServer, DatabaseConnection, AppState, String) {
                    let base = mock_threads().await;
                    let mut config = Config::from_env().expect("test config");
                    config.trust_proxy = true;
                    let app = configured.then(|| ("tapp".to_owned(), "tsecret".to_owned()));
                    let mut t = Threads::new(app).unwrap();
                    t.graph_url = base.clone();
                    t.publish_wait = std::time::Duration::ZERO;
                    t.poll_every = std::time::Duration::ZERO;
                    config.threads = t;
                    let disk = object_store::local::LocalFileSystem::new_with_prefix(media_dir())
                        .unwrap()
                        .with_automatic_cleanup(true);
                    config.media = Media::new(std::sync::Arc::new(disk), true);
                    let db = socialreins_brain::connect(&config)
                        .await
                        .expect("database reachable");
                    let state = AppState::new(db.clone(), &config);
                    let app = controllers::router(state.clone(), &config);
                    (TestServer::new(app).unwrap(), db, state, base)
                }

                /// The query of a redirect's `Location`.
                pub(super) fn location(
                    res: &axum_test::TestResponse,
                ) -> (String, HashMap<String, String>) {
                    assert_eq!(res.status_code(), 303, "{}", res.text());
                    let to = res.header("location").to_str().unwrap().to_owned();
                    let url = reqwest::Url::parse(&to).unwrap();
                    let q = url.query_pairs().into_owned().collect();
                    (to, q)
                }

                /// Starts a connect: (state, `sr_threads=...`).
                async fn begin(s: &TestServer, cookie: &str) -> (String, String) {
                    let res = s
                        .get("/connections/threads/redirect")
                        .add_header("cookie", cookie)
                        .await;
                    let (to, q) = location(&res);
                    assert!(
                        to.starts_with("https://threads.com/oauth/authorize?"),
                        "{to}"
                    );
                    assert_eq!(q["client_id"], "tapp");
                    assert_eq!(
                        q["redirect_uri"],
                        "http://localhost:8080/connect/threads/callback"
                    );
                    assert_eq!(
                        q["scope"],
                        "threads_basic,threads_content_publish,threads_delete"
                    );
                    let set = res
                        .iter_headers_by_name("set-cookie")
                        .map(|v| v.to_str().unwrap().to_owned())
                        .find(|c| c.starts_with("sr_threads="))
                        .unwrap();
                    assert!(set.contains("HttpOnly") && set.contains("Path=/connect/threads"));
                    (
                        q["state"].clone(),
                        set.split(';').next().unwrap().to_owned(),
                    )
                }

                /// Threads' redirect back: where it lands, as `(connected, connect_error)`.
                async fn finish(
                    s: &TestServer,
                    cookies: &str,
                    query: &str,
                ) -> (Option<String>, Option<String>) {
                    let res = s
                        .get(&format!("/connect/threads/callback?{query}"))
                        .add_header("cookie", cookies)
                        .await;
                    let (to, q) = location(&res);
                    assert!(
                        to.starts_with("http://localhost:3000/app/connections?"),
                        "{to}"
                    );
                    (q.get("connected").cloned(), q.get("connect_error").cloned())
                }

                /// Connects the mock's profile to the caller's current team.
                async fn connect_threads(s: &TestServer, cookie: &str) {
                    let (state, flow) = begin(s, cookie).await;
                    let both = format!("{cookie}; {flow}");
                    let got = finish(s, &both, &format!("code=good&state={state}")).await;
                    assert_eq!(got, (Some("threads".into()), None));
                }

                pub(super) async fn set_role(
                    db: &DatabaseConnection,
                    user: uuid::Uuid,
                    role: &str,
                ) {
                    use sea_orm::{ConnectionTrait, DbBackend, Statement};
                    db.execute_raw(Statement::from_sql_and_values(
                        DbBackend::Postgres,
                        "UPDATE team_user SET role = $2 WHERE user_id = $1",
                        [user.into(), role.into()],
                    ))
                    .await
                    .unwrap();
                }

                async fn threads_account(
                    db: &DatabaseConnection,
                    team_id: &str,
                ) -> social_accounts::Model {
                    social_accounts::Entity::find()
                        .filter(
                            social_accounts::Column::TeamId
                                .eq(team_id.parse::<uuid::Uuid>().unwrap()),
                        )
                        .filter(social_accounts::Column::Provider.eq("threads"))
                        .one(db)
                        .await
                        .unwrap()
                        .unwrap()
                }

                #[tokio::test]
                #[serial]
                async fn connect_saves_the_profile_once_in_the_default_set() {
                    let (s, db, _, _) = on_threads(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let team_id = current(&s, &cookie).await["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    let (state, flow) = begin(&s, &cookie).await;
                    let both = format!("{cookie}; {flow}");
                    let query = format!("code=good&state={state}");
                    assert_eq!(
                        finish(&s, &both, &query).await,
                        (Some("threads".into()), None)
                    );
                    let listed = list(&s, &cookie).await;
                    assert_eq!(listed.len(), 1);
                    let a = &listed[0];
                    assert_eq!(a["provider"], "threads");
                    assert_eq!(a["external_id"], "1789");
                    assert_eq!(a["handle"], "ann");
                    assert_eq!(a["display_name"], "Ann Lee");
                    let set = default_set(&db, &team_id).await.to_string();
                    assert_eq!(a["set_ids"], json!([set]));
                    let row = threads_account(&db, &team_id).await;
                    assert_eq!(cipher().decrypt(&row.access_token).unwrap(), b"long");
                    let left = row.token_expires_at.unwrap().timestamp() - Utc::now().timestamp();
                    assert!((5_183_000..=5_184_000).contains(&left), "{left}");
                    // The state works once.
                    assert_eq!(
                        finish(&s, &both, &query).await,
                        (None, Some("expired".into()))
                    );
                    // A reconnect keeps the one row.
                    connect_threads(&s, &cookie).await;
                    let again = list(&s, &cookie).await;
                    assert_eq!(again.len(), 1);
                    assert_eq!(again[0]["id"], a["id"]);
                }

                #[tokio::test]
                #[serial]
                async fn connect_refuses_strangers_members_and_other_browsers() {
                    let (s, db, _, _) = on_threads(true).await;
                    let (_, cookie, user_id) = user(&s, &db).await;
                    let back = |res: axum_test::TestResponse| location(&res).1;
                    let res = s.get("/connections/threads/redirect").await;
                    assert_eq!(back(res)["connect_error"], "unauthorized");

                    // The state must come back to the browser that started it.
                    let (state, flow) = begin(&s, &cookie).await;
                    let query = format!("code=good&state={state}");
                    assert_eq!(
                        finish(&s, &cookie, &query).await,
                        (None, Some("expired".into()))
                    );
                    let other = "sr_threads=0000";
                    let wrong = format!("{cookie}; {other}");
                    assert_eq!(
                        finish(&s, &wrong, &query).await,
                        (None, Some("expired".into()))
                    );
                    // Those did not use it up; another signed-in user cannot finish it.
                    let (_, stranger, _) = user(&s, &db).await;
                    let theirs = format!("{stranger}; {flow}");
                    assert_eq!(
                        finish(&s, &theirs, &query).await,
                        (None, Some("expired".into()))
                    );

                    // Cancelled on Threads, a bad code, and a demotion meanwhile.
                    let (state, flow) = begin(&s, &cookie).await;
                    let both = format!("{cookie}; {flow}");
                    let q = format!("error=access_denied&state={state}");
                    assert_eq!(
                        finish(&s, &both, &q).await,
                        (None, Some("cancelled".into()))
                    );
                    let (state, flow) = begin(&s, &cookie).await;
                    let both = format!("{cookie}; {flow}");
                    let q = format!("code=bad&state={state}");
                    assert_eq!(
                        finish(&s, &both, &q).await,
                        (None, Some("threads_failed".into()))
                    );
                    let (state, flow) = begin(&s, &cookie).await;
                    set_role(&db, user_id, "member").await;
                    let both = format!("{cookie}; {flow}");
                    let q = format!("code=good&state={state}");
                    assert_eq!(finish(&s, &both, &q).await, (None, Some("expired".into())));
                    let res = s
                        .get("/connections/threads/redirect")
                        .add_header("cookie", &cookie)
                        .await;
                    assert_eq!(back(res)["connect_error"], "forbidden");
                    assert!(list(&s, &cookie).await.is_empty());

                    let (s, db, _, _) = on_threads(false).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let res = s
                        .get("/connections/threads/redirect")
                        .add_header("cookie", &cookie)
                        .await;
                    assert_eq!(back(res)["connect_error"], "not_configured");
                }

                fn threads_post(
                    set: &str,
                    body: &str,
                    media: Vec<String>,
                    parts: &[&str],
                ) -> Value {
                    let mut v = input(set, body, Some(&past()));
                    v["platform"] = json!("threads");
                    v["media"] = json!(media);
                    v["thread"] = parts.iter().map(|p| json!({"body": p})).collect();
                    v
                }

                #[tokio::test]
                #[serial]
                async fn posts_publish_as_containers_and_come_down() {
                    let (s, db, state, base) = on_threads(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let team_id = current(&s, &cookie).await["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    connect_threads(&s, &cookie).await;
                    let set = default_set(&db, &team_id).await.to_string();
                    let body = threads_post(&set, "one", vec![], &["two"]);
                    let id = id_of(&create(&s, &cookie, body).await);
                    // Plain http on loopback, which only tests allow, so set past validation.
                    set_media(&db, &base, &id, &["/img.png"]).await;
                    let before = CONTAINERS.lock().unwrap().len();
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "published", "{p}");
                    let made: Vec<HashMap<String, String>> = CONTAINERS.lock().unwrap()[before..]
                        .iter()
                        .map(|(user, f)| {
                            assert_eq!(user, "1789");
                            f.clone()
                        })
                        .collect();
                    assert_eq!(made.len(), 2);
                    assert_eq!(made[0]["media_type"], "IMAGE");
                    assert_eq!(made[0]["text"], "one");
                    assert!(!made[0].contains_key("reply_to_id"));
                    let link = made[0]["image_url"].clone();
                    assert!(link.starts_with("http://localhost:8080/media/"), "{link}");
                    assert!(link.ends_with(".png"));
                    let t = &p["targets"][0];
                    let root = t["remote_id"].as_str().unwrap().to_owned();
                    assert_eq!(
                        t["remote_url"],
                        format!("https://www.threads.com/@ann/post/P{root}")
                    );
                    assert_eq!(made[1]["media_type"], "TEXT");
                    assert_eq!(made[1]["text"], "two");
                    assert_eq!(made[1]["reply_to_id"], root);
                    let part = t["parts"][0]["remote_id"].as_str().unwrap().to_owned();
                    assert!(t["parts"][0]["remote_url"]
                        .as_str()
                        .unwrap()
                        .ends_with(&format!("/P{part}")));
                    // The post settled, so its media link is dead.
                    let path = link.trim_start_matches("http://localhost:8080");
                    assert_eq!(s.get(path).await.status_code(), 404);

                    let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                    res.assert_status_ok();
                    assert_eq!(res.json::<Value>()["status"], "deleted");
                    let removed = REMOVED.lock().unwrap().clone();
                    assert_eq!(removed[removed.len() - 2..], [part, root]);
                }

                #[tokio::test]
                #[serial]
                async fn carousels_videos_and_a_refused_token() {
                    let (s, db, state, base) = on_threads(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let team_id = current(&s, &cookie).await["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    connect_threads(&s, &cookie).await;
                    let set = default_set(&db, &team_id).await.to_string();
                    let id =
                        id_of(&create(&s, &cookie, threads_post(&set, "c", vec![], &[])).await);
                    set_media(&db, &base, &id, &["/img.png", "/clip.mp4"]).await;
                    let before = CONTAINERS.lock().unwrap().len();
                    publish::tick(&state).await.unwrap();
                    assert_eq!(get_post(&s, &cookie, &id).await["status"], "published");
                    let made: Vec<HashMap<String, String>> = CONTAINERS.lock().unwrap()[before..]
                        .iter()
                        .map(|(_, f)| f.clone())
                        .collect();
                    assert_eq!(made.len(), 3);
                    assert_eq!(made[0]["media_type"], "IMAGE");
                    assert_eq!(made[0]["is_carousel_item"], "true");
                    assert_eq!(made[1]["media_type"], "VIDEO");
                    assert!(made[1]["video_url"].ends_with(".mp4"));
                    assert_eq!(made[2]["media_type"], "CAROUSEL");
                    assert_eq!(made[2]["text"], "c");
                    assert_eq!(made[2]["children"].split(',').count(), 2);
                    assert!(!PUBLISHED.lock().unwrap().is_empty());

                    // Too many items never reach Threads.
                    let many = vec!["https://a.example/i.png".to_owned(); 21];
                    let res = create(&s, &cookie, threads_post(&set, "x", many, &[])).await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json(&json!({"error": "too_many_images"}));

                    // A token Threads refuses fails the target with what to do.
                    let account = threads_account(&db, &team_id).await;
                    let row = social_accounts::ActiveModel {
                        id: Set(account.id),
                        access_token: Set(cipher().encrypt(b"dead")),
                        ..Default::default()
                    };
                    social_accounts::Entity::update(row)
                        .exec(&db)
                        .await
                        .unwrap();
                    let id =
                        id_of(&create(&s, &cookie, threads_post(&set, "x", vec![], &[])).await);
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "failed");
                    assert_eq!(p["targets"][0]["error"], publish::THREADS_RECONNECT);
                }

                #[tokio::test]
                #[serial]
                async fn the_daily_refresh_extends_tokens_near_expiry() {
                    let (s, db, state, _) = on_threads(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let team_id = current(&s, &cookie).await["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    connect_threads(&s, &cookie).await;
                    let account = threads_account(&db, &team_id).await;
                    let expire_in = |days: i64| {
                        let row = social_accounts::ActiveModel {
                            id: Set(account.id),
                            token_expires_at: Set(Some(
                                (Utc::now() + Duration::days(days)).fixed_offset(),
                            )),
                            ..Default::default()
                        };
                        social_accounts::Entity::update(row).exec(&db)
                    };
                    // Over 30 days left, or already expired: left alone.
                    for days in [40, -1] {
                        expire_in(days).await.unwrap();
                        threads::refresh_tokens(&state).await.unwrap();
                        let row = threads_account(&db, &team_id).await;
                        assert_eq!(cipher().decrypt(&row.access_token).unwrap(), b"long");
                    }
                    expire_in(10).await.unwrap();
                    assert!(threads::refresh_tokens(&state).await.unwrap() >= 1);
                    let row = threads_account(&db, &team_id).await;
                    assert_eq!(cipher().decrypt(&row.access_token).unwrap(), b"refreshed");
                    let left = row.token_expires_at.unwrap() - Utc::now().fixed_offset();
                    assert!(left > Duration::days(59), "{left}");
                }

                #[tokio::test]
                #[serial]
                async fn stored_media_is_served_at_its_link_until_released() {
                    let (s, db, state, base) = on_threads(true).await;
                    let post = uuid::Uuid::now_v7();
                    let until = tokio::time::Instant::now() + media::INGEST_DEADLINE;
                    let url = format!("{base}/img.png");
                    let stored = state
                        .media
                        .ingest(&db, post, &url, 1_000_000, until)
                        .await
                        .unwrap();
                    let link = media::link(&state, &stored);
                    let path = link.trim_start_matches("http://localhost:8080").to_owned();
                    let res = s.get(&path).await;
                    res.assert_status_ok();
                    assert_eq!(res.header("content-type"), "image/png");
                    assert_eq!(res.header("cache-control"), "no-store");
                    assert_eq!(res.as_bytes().len(), 64);
                    // Only brain's own sealed post keys open anything.
                    assert_eq!(s.get("/media/nonsense.png").await.status_code(), 404);
                    let token = path.trim_start_matches("/media/");
                    let flipped: String = token
                        .chars()
                        .enumerate()
                        .map(|(i, c)| {
                            if i == 5 {
                                if c == 'A' {
                                    'B'
                                } else {
                                    'A'
                                }
                            } else {
                                c
                            }
                        })
                        .collect();
                    assert_eq!(s.get(&format!("/media/{flipped}")).await.status_code(), 404);
                    let other = URL_SAFE_NO_PAD.encode(cipher().encrypt(b"users/secret"));
                    assert_eq!(s.get(&format!("/media/{other}")).await.status_code(), 404);
                    state.media.release_post(&db, post).await;
                    assert_eq!(s.get(&path).await.status_code(), 404);
                }
            }

            /// LinkedIn: the connect flow, publishing through the Posts, Images and Videos APIs
            /// mock, take down, expired tokens and the daily token renewal.
            mod linkedin_api {
                use super::threads_api::{location, set_role};
                use super::*;
                use axum::{
                    extract::{Form, Path, Query},
                    http::{HeaderMap, StatusCode, Uri},
                    response::IntoResponse,
                    routing::{get, post, put},
                    Json, Router,
                };
                use socialreins_brain::{
                    connectors::linkedin::{LinkedIn, API_VERSION},
                    controllers::linkedin,
                };
                use std::sync::atomic::{AtomicU64, Ordering};

                /// Every `/rest/posts` body the mock took, oldest first.
                static POSTS: std::sync::Mutex<Vec<Value>> = std::sync::Mutex::new(Vec::new());
                /// Every upload: (path, byte count), and every finalize body.
                static UPLOADS: std::sync::Mutex<Vec<(String, usize)>> =
                    std::sync::Mutex::new(Vec::new());
                static FINALIZED: std::sync::Mutex<Vec<Value>> = std::sync::Mutex::new(Vec::new());
                /// Every comment the mock took: (path, body). `COMMENT_MODE` 1: LinkedIn
                /// refuses it for a missing permission.
                static COMMENTS: std::sync::Mutex<Vec<(String, Value)>> =
                    std::sync::Mutex::new(Vec::new());
                static COMMENT_MODE: AtomicU64 = AtomicU64::new(0);
                /// The raw path of every delete.
                static REMOVED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
                static NEXT: AtomicU64 = AtomicU64::new(1);
                /// When set, the mock hands out this upload URL instead of its own.
                static FORCED_URL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
                /// Requests the "foreign host" took.
                static FOREIGN_HITS: AtomicU64 = AtomicU64::new(0);
                /// Every document initialize body the mock took.
                static DOC_INITS: std::sync::Mutex<Vec<Value>> = std::sync::Mutex::new(Vec::new());
                /// How the mock answers a document status check: 0 AVAILABLE after one
                /// PROCESSING answer, 1 PROCESSING_FAILED.
                static DOC_MODE: AtomicU64 = AtomicU64::new(0);
                static DOC_POLLS: AtomicU64 = AtomicU64::new(0);

                fn forced_url() -> Option<String> {
                    FORCED_URL.lock().unwrap().clone()
                }

                type Answer = (StatusCode, Json<Value>);

                fn ok(v: Value) -> Answer {
                    (StatusCode::OK, Json(v))
                }

                fn expired() -> Answer {
                    let msg = "The token used in the request has been revoked or is expired.";
                    (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({"message": msg, "status": 401})),
                    )
                }

                /// A token the mock handed out, with the version headers `/rest` needs.
                fn rest_ok(h: &HeaderMap) -> bool {
                    let bearer = h.get("authorization").and_then(|v| v.to_str().ok());
                    let version = h.get("linkedin-version").and_then(|v| v.to_str().ok());
                    let proto = h
                        .get("x-restli-protocol-version")
                        .and_then(|v| v.to_str().ok());
                    matches!(bearer, Some("Bearer lin" | "Bearer lrefreshed"))
                        && version == Some(API_VERSION)
                        && proto == Some("2.0.0")
                }

                fn next() -> u64 {
                    NEXT.fetch_add(1, Ordering::SeqCst)
                }

                /// LinkedIn (OAuth, userinfo, `/rest`, upload hosts), plus a PNG and an MP4.
                /// Code `good` gives a 60-day token, `partner` also a refresh token.
                async fn mock_linkedin() -> String {
                    *FORCED_URL.lock().unwrap() = None;
                    COMMENT_MODE.store(0, Ordering::SeqCst);
                    DOC_MODE.store(0, Ordering::SeqCst);
                    DOC_POLLS.store(0, Ordering::SeqCst);
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let base = format!("http://{}", listener.local_addr().unwrap());
                    let token = |Form(f): Form<HashMap<String, String>>| async move {
                        let get = |k: &str| f.get(k).map(String::as_str).unwrap_or_default();
                        let app = get("client_id") == "lapp" && get("client_secret") == "lsecret";
                        let code = (get("grant_type"), get("code"), get("refresh_token"));
                        match (app, code) {
                            (true, ("authorization_code", "good", _)) => {
                                ok(json!({"access_token": "lin", "expires_in": 5_184_000}))
                            }
                            (true, ("authorization_code", "partner", _)) => ok(json!({
                                "access_token": "lin", "expires_in": 5_184_000,
                                "refresh_token": "lrefresh", "refresh_token_expires_in": 31_536_000})),
                            (true, ("refresh_token", _, "lrefresh")) => ok(json!({
                                "access_token": "lrefreshed", "expires_in": 5_184_000,
                                "refresh_token": "lrefresh2"})),
                            _ => (
                                StatusCode::BAD_REQUEST,
                                Json(json!({"error": "invalid_request",
                                    "error_description": "The provided authorization code is invalid"})),
                            ),
                        }
                    };
                    let userinfo = |h: HeaderMap| async move {
                        let bearer = h.get("authorization").and_then(|v| v.to_str().ok());
                        if !matches!(bearer, Some("Bearer lin" | "Bearer lrefreshed")) {
                            return expired();
                        }
                        ok(json!({"sub": "abc123", "name": "Ann Lee",
                            "picture": "https://media.example/p.jpg"}))
                    };
                    let b = base.clone();
                    let images = move |h: HeaderMap, Json(body): Json<Value>| async move {
                        if !rest_ok(&h) {
                            return expired();
                        }
                        assert_eq!(
                            body["initializeUploadRequest"]["owner"],
                            "urn:li:person:abc123"
                        );
                        let n = next();
                        ok(
                            json!({"value": {"uploadUrl": forced_url().unwrap_or(format!("{b}/upload/img/{n}")),
                            "image": format!("urn:li:image:{n}")}}),
                        )
                    };
                    let b = base.clone();
                    let videos = move |h: HeaderMap,
                                       Query(q): Query<HashMap<String, String>>,
                                       Json(body): Json<Value>| async move {
                        if !rest_ok(&h) {
                            return expired();
                        }
                        match q.get("action").map(String::as_str) {
                            Some("initializeUpload") => {
                                let size = body["initializeUploadRequest"]["fileSizeBytes"]
                                    .as_u64()
                                    .unwrap();
                                let n = next();
                                let half = size / 2;
                                ok(json!({"value": {
                                    "video": format!("urn:li:video:{n}"), "uploadToken": "tok",
                                    "uploadInstructions": [
                                        {"firstByte": 0, "lastByte": half - 1,
                                         "uploadUrl": forced_url().unwrap_or(format!("{b}/upload/vid/{n}a"))},
                                        {"firstByte": half, "lastByte": size - 1,
                                         "uploadUrl": format!("{b}/upload/vid/{n}b")}]}}))
                            }
                            Some("finalizeUpload") => {
                                FINALIZED.lock().unwrap().push(body);
                                ok(json!({}))
                            }
                            _ => (
                                StatusCode::BAD_REQUEST,
                                Json(json!({"message": "bad action"})),
                            ),
                        }
                    };
                    let b = base.clone();
                    let documents =
                        move |h: HeaderMap,
                              Query(q): Query<HashMap<String, String>>,
                              Json(body): Json<Value>| async move {
                            if !rest_ok(&h)
                                || q.get("action").map(String::as_str) != Some("initializeUpload")
                            {
                                return expired();
                            }
                            DOC_INITS.lock().unwrap().push(body);
                            let n = next();
                            ok(
                                json!({"value": {"uploadUrl": forced_url().unwrap_or(format!("{b}/upload/doc/{n}")),
                            "document": format!("urn:li:document:{n}")}}),
                            )
                        };
                    let document = |h: HeaderMap, Path(_urn): Path<String>| async move {
                        if !rest_ok(&h) {
                            return expired();
                        }
                        let polls = DOC_POLLS.fetch_add(1, Ordering::SeqCst);
                        if DOC_MODE.load(Ordering::SeqCst) == 1 {
                            return ok(json!({"status": "PROCESSING_FAILED",
                                "processingFailureReason": "The document has more than 300 pages"}));
                        }
                        ok(json!({"status": if polls == 0 { "PROCESSING" } else { "AVAILABLE" }}))
                    };
                    let video = |h: HeaderMap, Path(_urn): Path<String>| async move {
                        if !rest_ok(&h) {
                            return expired();
                        }
                        ok(json!({"status": "AVAILABLE"}))
                    };
                    let upload_img = |h: HeaderMap, uri: Uri, body: axum::body::Bytes| async move {
                        // Images and documents go with the token, video parts without.
                        let bearer = h.get("authorization").is_some();
                        let is_img = uri.path().starts_with("/upload/img/")
                            || uri.path().starts_with("/upload/doc/");
                        assert_eq!(bearer, is_img, "{}", uri.path());
                        UPLOADS
                            .lock()
                            .unwrap()
                            .push((uri.path().to_owned(), body.len()));
                        let tag = format!("\"etag-{}\"", uri.path().rsplit('/').next().unwrap());
                        (StatusCode::CREATED, [("etag", tag)])
                    };
                    let create = |h: HeaderMap, Json(body): Json<Value>| async move {
                        if !rest_ok(&h) {
                            return expired().into_response();
                        }
                        POSTS.lock().unwrap().push(body);
                        let id = format!("urn:li:share:{}", next());
                        (StatusCode::CREATED, [("x-restli-id", id)], "").into_response()
                    };
                    let comment = |h: HeaderMap, uri: Uri, Json(body): Json<Value>| async move {
                        if !rest_ok(&h) {
                            return expired().into_response();
                        }
                        if COMMENT_MODE.load(Ordering::SeqCst) == 1 {
                            let msg = "Not enough permissions to access: socialActions.CREATE";
                            return (
                                StatusCode::FORBIDDEN,
                                Json(json!({"message": msg, "status": 403})),
                            )
                                .into_response();
                        }
                        COMMENTS.lock().unwrap().push((uri.path().to_owned(), body));
                        let id = format!("cmt{}", next());
                        (StatusCode::CREATED, [("x-restli-id", id)], "").into_response()
                    };
                    let remove = |h: HeaderMap, uri: Uri| async move {
                        let method = h.get("x-restli-method").and_then(|v| v.to_str().ok());
                        assert_eq!(method, Some("DELETE"));
                        if !rest_ok(&h) {
                            return expired().into_response();
                        }
                        REMOVED.lock().unwrap().push(uri.path().to_owned());
                        StatusCode::NO_CONTENT.into_response()
                    };
                    let mut mp4 = vec![0u8; 64];
                    mp4[..12].copy_from_slice(b"\0\0\0\x18ftypisom");
                    let app = Router::new()
                        .route("/oauth/v2/accessToken", post(token))
                        .route("/v2/userinfo", get(userinfo))
                        .route("/rest/images", post(images))
                        .route("/rest/videos", post(videos))
                        .route("/rest/documents", post(documents))
                        .route("/rest/documents/{urn}", get(document))
                        .route("/rest/videos/{urn}", get(video))
                        .route("/upload/img/{n}", put(upload_img))
                        .route("/upload/vid/{n}", put(upload_img))
                        .route("/upload/doc/{n}", put(upload_img))
                        .route("/rest/posts", post(create))
                        .route("/rest/socialActions/{urn}/comments", post(comment))
                        .route("/rest/posts/{urn}", axum::routing::delete(remove))
                        .route("/img.png", get(|| async { image("image/png", 64) }))
                        .route("/img2.png", get(|| async { image("image/png", 48) }))
                        .route("/doc.pdf", get(|| async { image("application/pdf", 96) }))
                        .route("/page.html", get(|| async { image("text/html", 64) }))
                        .route(
                            "/pic.webp",
                            get(|| async {
                                let mut b = b"RIFF\x24\0\0\0WEBPVP8 ".to_vec();
                                b.resize(64, 0);
                                ([("content-type", "image/webp")], b)
                            }),
                        )
                        .route(
                            "/movie.mov",
                            get(|| async {
                                let mut b = b"\0\0\0\x14ftypqt  ".to_vec();
                                b.resize(64, 0);
                                ([("content-type", "video/quicktime")], b)
                            }),
                        )
                        .route(
                            "/clip.mp4",
                            get(move || async move { ([("content-type", "video/mp4")], mp4) }),
                        );
                    tokio::spawn(async move { axum::serve(listener, app).await });
                    base
                }

                /// A brain with LinkedIn set up on the mock (no waits), its state, and the mock.
                async fn on_linkedin(
                    configured: bool,
                ) -> (TestServer, DatabaseConnection, AppState, String) {
                    let base = mock_linkedin().await;
                    let mut config = Config::from_env().expect("test config");
                    config.trust_proxy = true;
                    let app = configured.then(|| ("lapp".to_owned(), "lsecret".to_owned()));
                    let mut l = LinkedIn::new(app).unwrap();
                    l.oauth_url = format!("{base}/oauth/v2/accessToken");
                    l.api_url = base.clone();
                    l.poll_every = std::time::Duration::ZERO;
                    config.linkedin = l;
                    let disk = object_store::local::LocalFileSystem::new_with_prefix(media_dir())
                        .unwrap()
                        .with_automatic_cleanup(true);
                    config.media = Media::new(std::sync::Arc::new(disk), true);
                    let db = socialreins_brain::connect(&config)
                        .await
                        .expect("database reachable");
                    let state = AppState::new(db.clone(), &config);
                    let app = controllers::router(state.clone(), &config);
                    (TestServer::new(app).unwrap(), db, state, base)
                }

                /// Starts a connect: (state, `sr_linkedin=...`).
                async fn begin(s: &TestServer, cookie: &str) -> (String, String) {
                    let res = s
                        .get("/connections/linkedin/redirect")
                        .add_header("cookie", cookie)
                        .await;
                    let (to, q) = location(&res);
                    assert!(
                        to.starts_with("https://www.linkedin.com/oauth/v2/authorization?"),
                        "{to}"
                    );
                    assert_eq!(q["client_id"], "lapp");
                    assert_eq!(q["response_type"], "code");
                    assert_eq!(
                        q["redirect_uri"],
                        "http://localhost:8080/connect/linkedin/callback"
                    );
                    assert_eq!(q["scope"], "openid profile email w_member_social");
                    let set = res
                        .iter_headers_by_name("set-cookie")
                        .map(|v| v.to_str().unwrap().to_owned())
                        .find(|c| c.starts_with("sr_linkedin="))
                        .unwrap();
                    assert!(set.contains("HttpOnly") && set.contains("Path=/connect/linkedin"));
                    (
                        q["state"].clone(),
                        set.split(';').next().unwrap().to_owned(),
                    )
                }

                /// LinkedIn's redirect back: where it lands, as `(connected, connect_error)`.
                async fn finish(
                    s: &TestServer,
                    cookies: &str,
                    query: &str,
                ) -> (Option<String>, Option<String>) {
                    let res = s
                        .get(&format!("/connect/linkedin/callback?{query}"))
                        .add_header("cookie", cookies)
                        .await;
                    let (to, q) = location(&res);
                    assert!(
                        to.starts_with("http://localhost:3000/app/connections?"),
                        "{to}"
                    );
                    if q.contains_key("connect_error") {
                        assert_eq!(q["provider"], "linkedin");
                    }
                    (q.get("connected").cloned(), q.get("connect_error").cloned())
                }

                /// Connects the mock's profile with `code` to the caller's current team.
                async fn connect_with(s: &TestServer, cookie: &str, code: &str) {
                    let (state, flow) = begin(s, cookie).await;
                    let both = format!("{cookie}; {flow}");
                    let got = finish(s, &both, &format!("code={code}&state={state}")).await;
                    assert_eq!(got, (Some("linkedin".into()), None));
                }

                async fn linkedin_account(
                    db: &DatabaseConnection,
                    team_id: &str,
                ) -> social_accounts::Model {
                    social_accounts::Entity::find()
                        .filter(
                            social_accounts::Column::TeamId
                                .eq(team_id.parse::<uuid::Uuid>().unwrap()),
                        )
                        .filter(social_accounts::Column::Provider.eq("linkedin"))
                        .one(db)
                        .await
                        .unwrap()
                        .unwrap()
                }

                /// Sets the account's token and expiry.
                async fn set_token(
                    db: &DatabaseConnection,
                    account: uuid::Uuid,
                    token: &str,
                    days: i64,
                ) {
                    let row = social_accounts::ActiveModel {
                        id: Set(account),
                        access_token: Set(cipher().encrypt(token.as_bytes())),
                        token_expires_at: Set(Some(
                            (Utc::now() + Duration::days(days)).fixed_offset(),
                        )),
                        ..Default::default()
                    };
                    social_accounts::Entity::update(row).exec(db).await.unwrap();
                }

                fn linkedin_post(set: &str, body: &str) -> Value {
                    let mut v = input(set, body, Some(&past()));
                    v["platform"] = json!("linkedin");
                    v
                }

                /// A connected team: (server, db, state, mock base, cookie, team id, set id).
                async fn team() -> (
                    TestServer,
                    DatabaseConnection,
                    AppState,
                    String,
                    String,
                    String,
                    String,
                ) {
                    let (s, db, state, base) = on_linkedin(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let team_id = current(&s, &cookie).await["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    connect_with(&s, &cookie, "good").await;
                    let set = default_set(&db, &team_id).await.to_string();
                    (s, db, state, base, cookie, team_id, set)
                }

                #[tokio::test]
                #[serial]
                async fn connect_saves_the_profile_once_in_the_default_set() {
                    let (s, db, _, _) = on_linkedin(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let team_id = current(&s, &cookie).await["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    let (state, flow) = begin(&s, &cookie).await;
                    let both = format!("{cookie}; {flow}");
                    let query = format!("code=good&state={state}");
                    assert_eq!(
                        finish(&s, &both, &query).await,
                        (Some("linkedin".into()), None)
                    );
                    let listed = list(&s, &cookie).await;
                    assert_eq!(listed.len(), 1);
                    let a = &listed[0];
                    assert_eq!(a["provider"], "linkedin");
                    assert_eq!(a["external_id"], "abc123");
                    assert_eq!(a["display_name"], "Ann Lee");
                    assert_eq!(a["avatar_url"], "https://media.example/p.jpg");
                    let set = default_set(&db, &team_id).await.to_string();
                    assert_eq!(a["set_ids"], json!([set]));
                    let row = linkedin_account(&db, &team_id).await;
                    assert_eq!(cipher().decrypt(&row.access_token).unwrap(), b"lin");
                    assert_eq!(row.meta, json!({}));
                    let left = row.token_expires_at.unwrap().timestamp() - Utc::now().timestamp();
                    assert!((5_183_000..=5_184_000).contains(&left), "{left}");
                    // The state works once.
                    assert_eq!(
                        finish(&s, &both, &query).await,
                        (None, Some("expired".into()))
                    );
                    // A reconnect keeps the one row.
                    connect_with(&s, &cookie, "good").await;
                    let again = list(&s, &cookie).await;
                    assert_eq!(again.len(), 1);
                    assert_eq!(again[0]["id"], a["id"]);
                }

                #[tokio::test]
                #[serial]
                async fn a_partner_apps_refresh_token_is_kept_sealed() {
                    let (s, db, _, _) = on_linkedin(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let team_id = current(&s, &cookie).await["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    connect_with(&s, &cookie, "partner").await;
                    let row = linkedin_account(&db, &team_id).await;
                    let sealed = row.meta["refresh_token"].as_str().unwrap();
                    assert!(!sealed.contains("lrefresh"));
                    let bytes = URL_SAFE_NO_PAD.decode(sealed).unwrap();
                    assert_eq!(cipher().decrypt(&bytes).unwrap(), b"lrefresh");
                    // A later connect without one drops it.
                    connect_with(&s, &cookie, "good").await;
                    assert_eq!(linkedin_account(&db, &team_id).await.meta, json!({}));
                }

                #[tokio::test]
                #[serial]
                async fn connect_refuses_strangers_members_and_other_browsers() {
                    let (s, db, _, _) = on_linkedin(true).await;
                    let (_, cookie, user_id) = user(&s, &db).await;
                    let back = |res: axum_test::TestResponse| location(&res).1;
                    let res = s.get("/connections/linkedin/redirect").await;
                    assert_eq!(back(res)["connect_error"], "unauthorized");

                    // The state must come back to the browser that started it.
                    let (state, flow) = begin(&s, &cookie).await;
                    let query = format!("code=good&state={state}");
                    assert_eq!(
                        finish(&s, &cookie, &query).await,
                        (None, Some("expired".into()))
                    );
                    let wrong = format!("{cookie}; sr_linkedin=0000");
                    assert_eq!(
                        finish(&s, &wrong, &query).await,
                        (None, Some("expired".into()))
                    );
                    // Those did not use it up; another signed-in user cannot finish it.
                    let (_, stranger, _) = user(&s, &db).await;
                    let theirs = format!("{stranger}; {flow}");
                    assert_eq!(
                        finish(&s, &theirs, &query).await,
                        (None, Some("expired".into()))
                    );

                    // Cancelled on LinkedIn, a bad code, and a demotion meanwhile.
                    let (state, flow) = begin(&s, &cookie).await;
                    let both = format!("{cookie}; {flow}");
                    let q = format!("error=user_cancelled_login&state={state}");
                    assert_eq!(
                        finish(&s, &both, &q).await,
                        (None, Some("cancelled".into()))
                    );
                    let (state, flow) = begin(&s, &cookie).await;
                    let both = format!("{cookie}; {flow}");
                    let q = format!("code=bad&state={state}");
                    assert_eq!(
                        finish(&s, &both, &q).await,
                        (None, Some("linkedin_failed".into()))
                    );
                    let (state, flow) = begin(&s, &cookie).await;
                    set_role(&db, user_id, "member").await;
                    let both = format!("{cookie}; {flow}");
                    let q = format!("code=good&state={state}");
                    assert_eq!(finish(&s, &both, &q).await, (None, Some("expired".into())));
                    let res = s
                        .get("/connections/linkedin/redirect")
                        .add_header("cookie", &cookie)
                        .await;
                    assert_eq!(back(res)["connect_error"], "forbidden");
                    assert!(list(&s, &cookie).await.is_empty());

                    let (s, db, _, _) = on_linkedin(false).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let res = s
                        .get("/connections/linkedin/redirect")
                        .add_header("cookie", &cookie)
                        .await;
                    assert_eq!(back(res)["connect_error"], "not_configured");
                }

                #[tokio::test]
                #[serial]
                async fn a_text_post_publishes_and_comes_down() {
                    let (s, db, state, _, cookie, _, set) = team().await;
                    let body = linkedin_post(&set, "Hello (world) #rust *bold*");
                    let id = id_of(&create(&s, &cookie, body).await);
                    let before = POSTS.lock().unwrap().len();
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "published", "{p}");
                    let sent = POSTS.lock().unwrap()[before].clone();
                    assert_eq!(sent["author"], "urn:li:person:abc123");
                    assert_eq!(sent["commentary"], r"Hello \(world\) #rust \*bold\*");
                    assert_eq!(sent["visibility"], "PUBLIC");
                    assert_eq!(sent["lifecycleState"], "PUBLISHED");
                    assert_eq!(sent["distribution"]["feedDistribution"], "MAIN_FEED");
                    assert!(sent.get("content").is_none());
                    let t = &p["targets"][0];
                    let urn = t["remote_id"].as_str().unwrap().to_owned();
                    assert!(urn.starts_with("urn:li:share:"), "{urn}");
                    assert_eq!(
                        t["remote_url"],
                        format!("https://www.linkedin.com/feed/update/{urn}")
                    );

                    let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                    res.assert_status_ok();
                    assert_eq!(res.json::<Value>()["status"], "deleted");
                    let removed = REMOVED.lock().unwrap().clone();
                    let want = format!("/rest/posts/{}", urn.replace(':', "%3A"));
                    assert_eq!(removed.last(), Some(&want));
                    drop(db);
                }

                #[tokio::test]
                #[serial]
                async fn a_first_comment_follows_the_post_as_the_member_and_stays_when_it_comes_down(
                ) {
                    let (s, _, state, _, cookie, _, set) = team().await;
                    let mut body = linkedin_post(&set, "Big claim");
                    body["first_comment"] = json!("Sources (see #notes) *here*");
                    let id = id_of(&create(&s, &cookie, body).await);
                    let before = COMMENTS.lock().unwrap().len();
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "published", "{p}");
                    let t = &p["targets"][0];
                    let urn = t["remote_id"].as_str().unwrap().to_owned();
                    let sent = COMMENTS.lock().unwrap()[before..].to_vec();
                    assert_eq!(sent.len(), 1);
                    let path = format!("/rest/socialActions/{}/comments", urn.replace(':', "%3A"));
                    assert_eq!(sent[0].0, path);
                    // Plain text, as written: the comment API has no escaping.
                    assert_eq!(sent[0].1["message"]["text"], "Sources (see #notes) *here*");
                    assert_eq!(sent[0].1["actor"], "urn:li:person:abc123");
                    assert_eq!(sent[0].1["object"], urn);
                    assert_eq!(t["comment_status"], "posted");
                    assert!(t["comment_id"].as_str().unwrap().starts_with("cmt"));

                    let removed = REMOVED.lock().unwrap().len();
                    let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                    res.assert_status_ok();
                    assert_eq!(res.json::<Value>()["status"], "deleted");
                    // One delete, of the post: nothing for the comment.
                    let gone = REMOVED.lock().unwrap()[removed..].to_vec();
                    assert_eq!(gone, [format!("/rest/posts/{}", urn.replace(':', "%3A"))]);
                }

                #[tokio::test]
                #[serial]
                async fn a_refused_first_comment_is_a_comment_error_not_a_failed_post() {
                    let (s, db, state, _, cookie, _, set) = team().await;
                    COMMENT_MODE.store(1, Ordering::SeqCst);
                    let mut body = linkedin_post(&set, "Hello");
                    body["first_comment"] = json!("Sources");
                    let id = id_of(&create(&s, &cookie, body).await);
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(p["error"], Value::Null);
                    let t = &p["targets"][0];
                    assert_eq!(
                        (t["status"].as_str(), t["error"].clone()),
                        (Some("published"), Value::Null)
                    );
                    assert_eq!(t["comment_status"], "failed");
                    let why = t["comment_error"].as_str().unwrap();
                    assert!(why.contains("w_member_social_feed"), "{why}");
                    // Not a reconnect prompt: the 403 is the app's permission, not the token.
                    assert!(!why.contains("Reconnect the account under"), "{why}");

                    // Once the permission is there, nothing retries it by itself. The target is
                    // sent again (queued, pending, unsent): only the `comment_status IS NULL`
                    // guard of the publishing statement keeps it from claiming the comment.
                    COMMENT_MODE.store(0, Ordering::SeqCst);
                    requeue_unsent(&db, &id).await;
                    let (posts, comments) =
                        (POSTS.lock().unwrap().len(), COMMENTS.lock().unwrap().len());
                    publish::publish_post(&state, id.parse().unwrap())
                        .await
                        .unwrap();
                    assert_eq!(POSTS.lock().unwrap().len(), posts + 1, "sent again");
                    assert_eq!(COMMENTS.lock().unwrap().len(), comments);
                    let again = get_post(&s, &cookie, &id).await;
                    assert_eq!(again["targets"][0]["status"], "published");
                    assert_eq!(again["targets"][0]["comment_status"], "failed");
                }

                #[tokio::test]
                #[serial]
                async fn a_target_is_published_and_claimed_for_its_comment_in_one_statement() {
                    use sea_orm::ConnectionTrait;
                    let (s, db, state, _, cookie, _, set) = team().await;
                    // A published target of a post with a first comment must never exist with
                    // no comment_status: this trigger refuses a statement that leaves one.
                    let guard = "DROP TRIGGER IF EXISTS fc_atomic ON post_targets;
                        CREATE OR REPLACE FUNCTION fc_atomic() RETURNS trigger AS $$ BEGIN
                          IF NEW.status = 'published' AND NEW.comment_status IS NULL
                             AND EXISTS (SELECT 1 FROM posts WHERE id = NEW.post_id
                                         AND first_comment IS NOT NULL) THEN
                            RAISE EXCEPTION 'published without a comment claim';
                          END IF;
                          RETURN NEW;
                        END $$ LANGUAGE plpgsql;
                        CREATE TRIGGER fc_atomic BEFORE UPDATE ON post_targets
                          FOR EACH ROW EXECUTE FUNCTION fc_atomic();";
                    db.execute_unprepared(guard).await.unwrap();
                    let mut body = linkedin_post(&set, "Hello");
                    body["first_comment"] = json!("Sources");
                    let id = id_of(&create(&s, &cookie, body).await);
                    let before = COMMENTS.lock().unwrap().len();
                    publish::tick(&state).await.unwrap();
                    db.execute_unprepared("DROP TRIGGER fc_atomic ON post_targets")
                        .await
                        .unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(p["targets"][0]["comment_status"], "posted", "{p}");
                    assert_eq!(COMMENTS.lock().unwrap().len(), before + 1);
                }

                #[tokio::test]
                #[serial]
                async fn recovery_fails_a_cut_off_first_comment_and_never_sends_it() {
                    use socialreins_brain::models::post_targets;
                    let (s, db, state, _, cookie, _, set) = team().await;
                    let mut body = linkedin_post(&set, "Hello");
                    body["first_comment"] = json!("Sources");
                    let id = id_of(&create(&s, &cookie, body).await);
                    publish::tick(&state).await.unwrap();
                    let target = post_targets::Entity::find()
                        .filter(post_targets::Column::PostId.eq(id.parse::<uuid::Uuid>().unwrap()))
                        .one(&db)
                        .await
                        .unwrap()
                        .unwrap();
                    let old = (Utc::now() - Duration::minutes(30)).fixed_offset();
                    let calls = COMMENTS.lock().unwrap().len();

                    // Cut off during the comment call: it may have posted.
                    let cut = post_targets::ActiveModel {
                        id: Set(target.id),
                        comment_status: Set(Some("posting".into())),
                        comment_id: Set(None),
                        updated_at: Set(old),
                        ..Default::default()
                    };
                    post_targets::Entity::update(cut).exec(&db).await.unwrap();
                    publish::recover_stuck(&db).await.unwrap();
                    let t = &get_post(&s, &cookie, &id).await["targets"][0].clone();
                    assert_eq!(t["comment_status"], "failed");
                    let why = t["comment_error"].as_str().unwrap();
                    assert!(why.contains("may have posted"), "{why}");

                    // Cut off after the post went up but before it was saved as published.
                    let cut = post_targets::ActiveModel {
                        id: Set(target.id),
                        status: Set("publishing".into()),
                        comment_status: Set(None),
                        comment_error: Set(None),
                        updated_at: Set(old),
                        ..Default::default()
                    };
                    post_targets::Entity::update(cut).exec(&db).await.unwrap();
                    publish::recover_stuck(&db).await.unwrap();
                    let t = &get_post(&s, &cookie, &id).await["targets"][0].clone();
                    assert_eq!(t["status"], "published");
                    assert_eq!(t["comment_status"], "failed");
                    let why = t["comment_error"].as_str().unwrap();
                    assert!(why.starts_with("Not posted"), "{why}");
                    assert_eq!(COMMENTS.lock().unwrap().len(), calls);
                }

                #[tokio::test]
                #[serial]
                async fn images_upload_first_and_the_post_names_them() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    // One image is `content.media`.
                    let id = id_of(&create(&s, &cookie, linkedin_post(&set, "one")).await);
                    set_media(&db, &base, &id, &["/img.png"]).await;
                    let (posts, ups) = (POSTS.lock().unwrap().len(), UPLOADS.lock().unwrap().len());
                    publish::tick(&state).await.unwrap();
                    assert_eq!(get_post(&s, &cookie, &id).await["status"], "published");
                    let sent = POSTS.lock().unwrap()[posts].clone();
                    let image = sent["content"]["media"]["id"].as_str().unwrap().to_owned();
                    assert!(image.starts_with("urn:li:image:"), "{image}");
                    let uploads = UPLOADS.lock().unwrap()[ups..].to_vec();
                    assert_eq!(uploads.len(), 1);
                    assert_eq!(uploads[0].1, 64);
                    assert!(uploads[0].0.starts_with("/upload/img/"));

                    // Several are `content.multiImage`, in order.
                    let id = id_of(&create(&s, &cookie, linkedin_post(&set, "two")).await);
                    set_media(&db, &base, &id, &["/img.png", "/img2.png"]).await;
                    let (posts, ups) = (POSTS.lock().unwrap().len(), UPLOADS.lock().unwrap().len());
                    publish::tick(&state).await.unwrap();
                    assert_eq!(get_post(&s, &cookie, &id).await["status"], "published");
                    let sent = POSTS.lock().unwrap()[posts].clone();
                    let images = sent["content"]["multiImage"]["images"].as_array().unwrap();
                    assert_eq!(images.len(), 2);
                    assert_ne!(images[0]["id"], images[1]["id"]);
                    let sizes: Vec<usize> =
                        UPLOADS.lock().unwrap()[ups..].iter().map(|u| u.1).collect();
                    assert_eq!(sizes, [64, 48]);
                }

                #[tokio::test]
                #[serial]
                async fn a_video_uploads_in_parts_and_is_finalized() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let id = id_of(&create(&s, &cookie, linkedin_post(&set, "clip")).await);
                    set_media(&db, &base, &id, &["/clip.mp4"]).await;
                    let (posts, ups, fin) = (
                        POSTS.lock().unwrap().len(),
                        UPLOADS.lock().unwrap().len(),
                        FINALIZED.lock().unwrap().len(),
                    );
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "published", "{p}");
                    let sent = POSTS.lock().unwrap()[posts].clone();
                    let video = sent["content"]["media"]["id"].as_str().unwrap().to_owned();
                    assert!(video.starts_with("urn:li:video:"), "{video}");
                    let sizes: Vec<usize> =
                        UPLOADS.lock().unwrap()[ups..].iter().map(|u| u.1).collect();
                    assert_eq!(sizes, [32, 32]);
                    let done = FINALIZED.lock().unwrap()[fin].clone();
                    let f = &done["finalizeUploadRequest"];
                    assert_eq!(f["video"], video);
                    assert_eq!(f["uploadToken"], "tok");
                    let ids = f["uploadedPartIds"].as_array().unwrap();
                    assert_eq!(ids.len(), 2);
                    assert!(ids[0].as_str().unwrap().starts_with("etag-"));
                }

                #[tokio::test]
                #[serial]
                async fn a_pdf_posts_as_a_document_titled_by_the_first_line_and_comes_down() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let body = "Q3 results (draft)\nSwipe through";
                    let id = id_of(&create(&s, &cookie, linkedin_post(&set, body)).await);
                    set_media(&db, &base, &id, &["/doc.pdf"]).await;
                    let (posts, ups, inits) = (
                        POSTS.lock().unwrap().len(),
                        UPLOADS.lock().unwrap().len(),
                        DOC_INITS.lock().unwrap().len(),
                    );
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "published", "{p}");
                    let init = DOC_INITS.lock().unwrap()[inits].clone();
                    assert_eq!(
                        init["initializeUploadRequest"]["owner"],
                        "urn:li:person:abc123"
                    );
                    let uploads = UPLOADS.lock().unwrap()[ups..].to_vec();
                    assert_eq!(uploads.len(), 1);
                    assert_eq!(uploads[0].1, 96);
                    assert!(uploads[0].0.starts_with("/upload/doc/"));
                    // Waited for AVAILABLE: one PROCESSING answer, then the next.
                    assert!(DOC_POLLS.load(Ordering::SeqCst) >= 2);
                    let sent = POSTS.lock().unwrap()[posts].clone();
                    let media = &sent["content"]["media"];
                    assert!(media["id"]
                        .as_str()
                        .unwrap()
                        .starts_with("urn:li:document:"));
                    assert_eq!(media["title"], "Q3 results (draft)");
                    assert_eq!(POSTS.lock().unwrap().len(), posts + 1);

                    // No text: the fixed title.
                    let id2 = id_of(&create(&s, &cookie, linkedin_post(&set, "x")).await);
                    set_media(&db, &base, &id2, &["/doc.pdf"]).await;
                    let blank = posts::ActiveModel {
                        id: Set(id2.parse().unwrap()),
                        body: Set("  \n ".into()),
                        ..Default::default()
                    };
                    posts::Entity::update(blank).exec(&db).await.unwrap();
                    let posts = POSTS.lock().unwrap().len();
                    publish::tick(&state).await.unwrap();
                    assert_eq!(get_post(&s, &cookie, &id2).await["status"], "published");
                    let sent = POSTS.lock().unwrap()[posts].clone();
                    assert_eq!(sent["content"]["media"]["title"], "Document");

                    // Take down is the post's delete, nothing for the document.
                    let urn = p["targets"][0]["remote_id"].as_str().unwrap().to_owned();
                    let removed = REMOVED.lock().unwrap().len();
                    let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                    res.assert_status_ok();
                    assert_eq!(res.json::<Value>()["status"], "deleted");
                    let gone = REMOVED.lock().unwrap()[removed..].to_vec();
                    assert_eq!(gone, [format!("/rest/posts/{}", urn.replace(':', "%3A"))]);
                }

                #[tokio::test]
                #[serial]
                async fn a_pdf_next_to_other_media_or_another_pdf_never_reaches_linkedin() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let want = "A LinkedIn document post takes exactly one PDF and nothing else.";
                    for media in [
                        ["/doc.pdf", "/img.png"],
                        ["/img.png", "/doc.pdf"],
                        ["/doc.pdf", "/clip.mp4"],
                        ["/doc.pdf", "/doc.pdf?2"],
                    ] {
                        let id = id_of(&create(&s, &cookie, linkedin_post(&set, "mix")).await);
                        set_media(&db, &base, &id, &media).await;
                        let (posts, inits, ups) = (
                            POSTS.lock().unwrap().len(),
                            DOC_INITS.lock().unwrap().len(),
                            UPLOADS.lock().unwrap().len(),
                        );
                        publish::tick(&state).await.unwrap();
                        let p = get_post(&s, &cookie, &id).await;
                        assert_eq!(p["status"], "failed", "{media:?}");
                        assert_eq!(p["targets"][0]["error"], want, "{media:?}");
                        assert_eq!(POSTS.lock().unwrap().len(), posts);
                        assert_eq!(DOC_INITS.lock().unwrap().len(), inits);
                        assert_eq!(UPLOADS.lock().unwrap().len(), ups);
                    }
                }

                #[tokio::test]
                #[serial]
                async fn a_file_that_is_not_a_pdf_image_or_video_is_refused_before_linkedin() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let want = "Image 1 is not a JPEG, PNG or GIF image, an MP4 video or a PDF.";
                    // An HTML page fails at storing; a stored WebP or MOV, at the publisher.
                    for file in ["/page.html", "/pic.webp", "/movie.mov"] {
                        let id = id_of(&create(&s, &cookie, linkedin_post(&set, "page")).await);
                        set_media(&db, &base, &id, &[file]).await;
                        let (inits, posts) =
                            (DOC_INITS.lock().unwrap().len(), POSTS.lock().unwrap().len());
                        publish::tick(&state).await.unwrap();
                        let p = get_post(&s, &cookie, &id).await;
                        assert_eq!(p["status"], "failed", "{file}");
                        assert_eq!(p["targets"][0]["error"], want, "{file}");
                        assert_eq!(DOC_INITS.lock().unwrap().len(), inits);
                        assert_eq!(POSTS.lock().unwrap().len(), posts);
                    }
                }

                #[tokio::test]
                #[serial]
                async fn a_document_linkedin_refuses_fails_the_target_and_is_not_repeated() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    DOC_MODE.store(1, Ordering::SeqCst);
                    let id = id_of(&create(&s, &cookie, linkedin_post(&set, "long one")).await);
                    set_media(&db, &base, &id, &["/doc.pdf"]).await;
                    let (posts, inits) =
                        (POSTS.lock().unwrap().len(), DOC_INITS.lock().unwrap().len());
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "failed", "{p}");
                    let why = p["targets"][0]["error"].as_str().unwrap();
                    assert_eq!(
                        why,
                        "LinkedIn could not process the document: The document has more than 300 pages"
                    );
                    assert_eq!(POSTS.lock().unwrap().len(), posts);
                    assert_eq!(DOC_INITS.lock().unwrap().len(), inits + 1);
                    // Later ticks leave a failed post alone: nothing is sent again.
                    publish::tick(&state).await.unwrap();
                    assert_eq!(DOC_INITS.lock().unwrap().len(), inits + 1);
                    assert_eq!(POSTS.lock().unwrap().len(), posts);
                }

                #[tokio::test]
                #[serial]
                async fn limits_and_unsupported_shapes_are_refused() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let res = create(&s, &cookie, linkedin_post(&set, &"a".repeat(3001))).await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json(&json!({"error": "too_long"}));
                    let mut exact = linkedin_post(&set, &"a".repeat(3000));
                    exact["scheduled_at"] = Value::Null;
                    assert_eq!(create(&s, &cookie, exact).await.status_code(), 201);
                    let mut many = linkedin_post(&set, "x");
                    many["media"] = json!(vec!["https://a.example/i.png"; 21]);
                    let res = create(&s, &cookie, many).await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json(&json!({"error": "too_many_images"}));
                    let mut threaded = linkedin_post(&set, "x");
                    threaded["thread"] = json!([{"body": "two"}]);
                    let res = create(&s, &cookie, threaded).await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json(&json!({"error": "thread_not_supported"}));

                    // A video next to an image never reaches LinkedIn.
                    let id = id_of(&create(&s, &cookie, linkedin_post(&set, "mix")).await);
                    set_media(&db, &base, &id, &["/img.png", "/clip.mp4"]).await;
                    let posts = POSTS.lock().unwrap().len();
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "failed");
                    assert!(p["targets"][0]["error"]
                        .as_str()
                        .unwrap()
                        .contains("one video, or images"));
                    assert_eq!(POSTS.lock().unwrap().len(), posts);
                }

                #[tokio::test]
                #[serial]
                async fn foreign_upload_urls_never_get_the_bytes_or_the_token() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    // A host that is not LinkedIn's: any request to it is counted.
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let port = listener.local_addr().unwrap().port();
                    let evil = format!("http://127.0.0.1:{port}");
                    let spy = Router::new().fallback(|| async {
                        FOREIGN_HITS.fetch_add(1, Ordering::SeqCst);
                        StatusCode::OK
                    });
                    tokio::spawn(async move { axum::serve(listener, spy).await });
                    let userinfo = format!("{base}@127.0.0.1:{port}/x");
                    let urls = [
                        format!("{evil}/x"),
                        userinfo,
                        "https://api.linkedin.com.evil.net/x".to_owned(),
                        "https://api.linkedin.com@evil.net/x".to_owned(),
                        "https://evil.net/www.linkedin.com".to_owned(),
                    ];
                    for (media, url) in [
                        ("/img.png", &urls[0]),
                        ("/clip.mp4", &urls[0]),
                        ("/doc.pdf", &urls[0]),
                        ("/doc.pdf", &urls[2]),
                    ]
                    .into_iter()
                    .chain(urls[1..].iter().map(|u| ("/img.png", u)))
                    {
                        *FORCED_URL.lock().unwrap() = Some(url.clone());
                        let id = id_of(&create(&s, &cookie, linkedin_post(&set, "x")).await);
                        set_media(&db, &base, &id, &[media]).await;
                        let posts = POSTS.lock().unwrap().len();
                        publish::tick(&state).await.unwrap();
                        let p = get_post(&s, &cookie, &id).await;
                        assert_eq!(p["status"], "failed", "{url}");
                        assert!(
                            p["targets"][0]["error"]
                                .as_str()
                                .unwrap()
                                .contains("unexpected upload URL"),
                            "{url}: {p}"
                        );
                        assert_eq!(POSTS.lock().unwrap().len(), posts, "{url}");
                    }
                    *FORCED_URL.lock().unwrap() = None;
                    assert_eq!(FOREIGN_HITS.load(Ordering::SeqCst), 0);
                }

                #[tokio::test]
                #[serial]
                async fn an_expired_or_refused_token_asks_for_a_reconnect() {
                    let (s, db, state, _, cookie, team_id, set) = team().await;
                    let account = linkedin_account(&db, &team_id).await.id;
                    // Past its 60 days: failed without a call.
                    set_token(&db, account, "lin", -1).await;
                    let id = id_of(&create(&s, &cookie, linkedin_post(&set, "a")).await);
                    let posts = POSTS.lock().unwrap().len();
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "failed");
                    assert_eq!(p["targets"][0]["error"], publish::LINKEDIN_RECONNECT);
                    assert_eq!(POSTS.lock().unwrap().len(), posts);
                    // Not yet expired but revoked: LinkedIn answers 401.
                    set_token(&db, account, "dead", 30).await;
                    let id = id_of(&create(&s, &cookie, linkedin_post(&set, "b")).await);
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "failed");
                    assert_eq!(p["targets"][0]["error"], publish::LINKEDIN_RECONNECT);
                    // The list still shows when it ends, for the reconnect prompt.
                    assert!(list(&s, &cookie).await[0]["token_expires_at"].is_string());
                }

                #[tokio::test]
                #[serial]
                async fn the_daily_renewal_covers_accounts_with_a_refresh_token_near_expiry() {
                    let (s, db, state, _) = on_linkedin(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let team_id = current(&s, &cookie).await["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    connect_with(&s, &cookie, "partner").await;
                    let account = linkedin_account(&db, &team_id).await.id;
                    // 40 days left: left alone.
                    set_token(&db, account, "lin", 40).await;
                    linkedin::refresh_tokens(&state).await.unwrap();
                    let row = linkedin_account(&db, &team_id).await;
                    assert_eq!(cipher().decrypt(&row.access_token).unwrap(), b"lin");
                    // 10 days left, or already expired: renewed, with the new refresh token.
                    for days in [10, -2] {
                        set_token(&db, account, "lin", days).await;
                        assert!(linkedin::refresh_tokens(&state).await.unwrap() >= 1);
                        let row = linkedin_account(&db, &team_id).await;
                        assert_eq!(cipher().decrypt(&row.access_token).unwrap(), b"lrefreshed");
                        let left = row.token_expires_at.unwrap() - Utc::now().fixed_offset();
                        assert!(left > Duration::days(59), "{left}");
                        let sealed = row.meta["refresh_token"].as_str().unwrap();
                        let bytes = URL_SAFE_NO_PAD.decode(sealed).unwrap();
                        assert_eq!(cipher().decrypt(&bytes).unwrap(), b"lrefresh2");
                        // The mock only renews "lrefresh": put it back for the next round.
                        let sealed = URL_SAFE_NO_PAD.encode(cipher().encrypt(b"lrefresh"));
                        let back = social_accounts::ActiveModel {
                            id: Set(account),
                            meta: Set(json!({ "refresh_token": sealed })),
                            ..Default::default()
                        };
                        social_accounts::Entity::update(back)
                            .exec(&db)
                            .await
                            .unwrap();
                    }
                    // Without a refresh token nothing is tried.
                    connect_with(&s, &cookie, "good").await;
                    set_token(&db, account, "lin", 5).await;
                    linkedin::refresh_tokens(&state).await.unwrap();
                    let row = linkedin_account(&db, &team_id).await;
                    assert_eq!(cipher().decrypt(&row.access_token).unwrap(), b"lin");
                }
            }

            /// YouTube: the connect flow on the Google client, resumable video uploads against a
            /// mock of the Data API, take down, token renewal and the session URL guard.
            mod youtube_api {
                use super::threads_api::{location, set_role};
                use super::*;
                use axum::{
                    extract::{Form, Query},
                    http::{HeaderMap, StatusCode},
                    response::IntoResponse,
                    routing::{delete, get, post},
                    Json, Router,
                };
                use socialreins_brain::connectors::youtube::YouTube;
                use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

                /// Every session start: (metadata, `X-Upload-Content-Length`, `X-Upload-Content-Type`).
                static STARTS: std::sync::Mutex<Vec<(Value, String, String)>> =
                    std::sync::Mutex::new(Vec::new());
                /// Every upload of bytes: (byte count, bearer token sent).
                static PUTS: std::sync::Mutex<Vec<(usize, String)>> =
                    std::sync::Mutex::new(Vec::new());
                /// The bearer token of every call that needs one, newest last.
                static BEARERS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
                /// Every video id the mock was asked to delete.
                static REMOVED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
                static REFRESHES: AtomicU64 = AtomicU64::new(0);
                static NEXT: AtomicU64 = AtomicU64::new(1);
                /// When set, the mock hands out this session URL instead of its own.
                static FORCED_URL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
                /// Requests the "foreign host" took.
                static FOREIGN_HITS: AtomicU64 = AtomicU64::new(0);
                /// The mock's quota is used up.
                static QUOTA: AtomicBool = AtomicBool::new(false);
                /// How the mock answers a delete: 0 as asked, 1 video gone, 2 scope missing.
                static DELETE_MODE: AtomicU64 = AtomicU64::new(0);
                /// Every comment thread the mock took: (bearer, query part, body).
                /// `COMMENT_MODE` 1: the video is still processing.
                static COMMENTS: std::sync::Mutex<Vec<(String, String, Value)>> =
                    std::sync::Mutex::new(Vec::new());
                static COMMENT_MODE: AtomicU64 = AtomicU64::new(0);
                /// Every thumbnail the mock took: (bearer, videoId, content type, byte count).
                /// `THUMB_MODE` 1: the channel is not verified (403 `forbidden`).
                static THUMBS: std::sync::Mutex<Vec<(String, String, String, usize)>> =
                    std::sync::Mutex::new(Vec::new());
                static THUMB_MODE: AtomicU64 = AtomicU64::new(0);

                type Answer = (StatusCode, Json<Value>);

                fn google_error(status: StatusCode, reason: &str, message: &str) -> Answer {
                    let error = json!({"code": status.as_u16(), "message": message,
                        "errors": [{"message": message, "reason": reason}]});
                    (status, Json(json!({ "error": error })))
                }

                fn expired() -> Answer {
                    let msg = "Invalid Credentials";
                    google_error(StatusCode::UNAUTHORIZED, "authError", msg)
                }

                /// A token the mock handed out; every call is logged.
                fn bearer_ok(h: &HeaderMap) -> bool {
                    let bearer = h.get("authorization").and_then(|v| v.to_str().ok());
                    BEARERS
                        .lock()
                        .unwrap()
                        .push(bearer.unwrap_or_default().to_owned());
                    matches!(bearer, Some("Bearer yt" | "Bearer yrefreshed"))
                }

                fn forced_url() -> Option<String> {
                    FORCED_URL.lock().unwrap().clone()
                }

                /// Google (token endpoint, channels, upload, delete), plus a PNG and an MP4.
                /// Code `good` gives a token with a refresh token, `norefresh` none,
                /// `nochannel` a token whose account has no channel.
                async fn mock_youtube() -> String {
                    *FORCED_URL.lock().unwrap() = None;
                    QUOTA.store(false, Ordering::SeqCst);
                    DELETE_MODE.store(0, Ordering::SeqCst);
                    COMMENT_MODE.store(0, Ordering::SeqCst);
                    THUMB_MODE.store(0, Ordering::SeqCst);
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let base = format!("http://{}", listener.local_addr().unwrap());
                    let token = |Form(f): Form<HashMap<String, String>>| async move {
                        let get = |k: &str| f.get(k).map(String::as_str).unwrap_or_default();
                        let app = get("client_id") == "yapp" && get("client_secret") == "ysecret";
                        let code = (get("grant_type"), get("code"), get("refresh_token"));
                        let bad = (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error": "invalid_grant",
                                "error_description": "Token has been expired or revoked."})),
                        );
                        let upload = "https://www.googleapis.com/auth/youtube.upload";
                        let read = "https://www.googleapis.com/auth/youtube.readonly";
                        let ssl = "https://www.googleapis.com/auth/youtube.force-ssl";
                        let scopes = format!("{upload} {read} {ssl}");
                        match (app, code) {
                            (true, ("authorization_code", "good", _)) => (
                                StatusCode::OK,
                                Json(json!({"access_token": "yt", "expires_in": 3599,
                                    "refresh_token": "yrefresh", "token_type": "Bearer",
                                    "scope": scopes})),
                            ),
                            // The user unticked a permission on Google's consent screen.
                            (true, ("authorization_code", "noforcessl", _)) => (
                                StatusCode::OK,
                                Json(json!({"access_token": "yt", "expires_in": 3599,
                                    "refresh_token": "yrefresh", "scope": format!("{upload} {read}")})),
                            ),
                            (true, ("authorization_code", "noupload", _)) => (
                                StatusCode::OK,
                                Json(json!({"access_token": "yt", "expires_in": 3599,
                                    "refresh_token": "yrefresh", "scope": format!("{read} {ssl}")})),
                            ),
                            (true, ("authorization_code", "noscope", _)) => (
                                StatusCode::OK,
                                Json(json!({"access_token": "yt", "expires_in": 3599,
                                    "refresh_token": "yrefresh"})),
                            ),
                            (true, ("authorization_code", "norefresh", _)) => (
                                StatusCode::OK,
                                Json(json!({"access_token": "yt", "expires_in": 3599,
                                    "scope": scopes})),
                            ),
                            (true, ("authorization_code", "nochannel", _)) => (
                                StatusCode::OK,
                                Json(json!({"access_token": "ynochannel", "expires_in": 3599,
                                    "refresh_token": "yrefresh", "scope": scopes})),
                            ),
                            (true, ("refresh_token", _, "yrefresh")) => {
                                REFRESHES.fetch_add(1, Ordering::SeqCst);
                                (
                                    StatusCode::OK,
                                    Json(json!({"access_token": "yrefreshed", "expires_in": 3599})),
                                )
                            }
                            _ => bad,
                        }
                    };
                    let channels = |h: HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                        assert_eq!(q["part"], "snippet");
                        assert_eq!(q["mine"], "true");
                        if h.get("authorization").and_then(|v| v.to_str().ok())
                            == Some("Bearer ynochannel")
                        {
                            return (
                                StatusCode::OK,
                                Json(json!({"kind": "youtube#channelListResponse",
                                    "pageInfo": {"totalResults": 0}})),
                            );
                        }
                        if !bearer_ok(&h) {
                            return expired();
                        }
                        (
                            StatusCode::OK,
                            Json(json!({"items": [{"id": "UCabc123", "snippet": {
                                "title": "Ann Lee Makes",
                                "customUrl": "@annleemakes",
                                "thumbnails": {
                                    "default": {"url": "https://yt3.ggpht.com/d=s88"},
                                    "medium": {"url": "https://yt3.ggpht.com/m=s240"}}}}]})),
                        )
                    };
                    let b = base.clone();
                    let start = move |h: HeaderMap,
                                      Query(q): Query<HashMap<String, String>>,
                                      Json(meta): Json<Value>| async move {
                        assert_eq!(q["uploadType"], "resumable");
                        assert_eq!(q["part"], "snippet,status");
                        if !bearer_ok(&h) {
                            return expired().into_response();
                        }
                        if QUOTA.load(Ordering::SeqCst) {
                            let msg = "The request cannot be completed because you have exceeded your quota.";
                            return google_error(StatusCode::FORBIDDEN, "quotaExceeded", msg)
                                .into_response();
                        }
                        let header = |k: &str| {
                            h.get(k)
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or_default()
                                .to_owned()
                        };
                        STARTS.lock().unwrap().push((
                            meta,
                            header("x-upload-content-length"),
                            header("x-upload-content-type"),
                        ));
                        let n = NEXT.fetch_add(1, Ordering::SeqCst);
                        let session = forced_url().unwrap_or(format!(
                            "{b}/upload/youtube/v3/videos?uploadType=resumable&upload_id=s{n}"
                        ));
                        (StatusCode::OK, [("location", session)], "").into_response()
                    };
                    let upload = |h: HeaderMap, body: axum::body::Bytes| async move {
                        let sent = h
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        PUTS.lock().unwrap().push((body.len(), sent));
                        let n = NEXT.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            Json(json!({"kind": "youtube#video", "id": format!("vid{n}"),
                                "status": {"privacyStatus": "public"}})),
                        )
                    };
                    let remove = |h: HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                        if !bearer_ok(&h) {
                            return expired().into_response();
                        }
                        match DELETE_MODE.load(Ordering::SeqCst) {
                            1 => {
                                return google_error(
                                    StatusCode::NOT_FOUND,
                                    "videoNotFound",
                                    "The video could not be found.",
                                )
                                .into_response()
                            }
                            2 => {
                                return google_error(
                                    StatusCode::FORBIDDEN,
                                    "insufficientPermissions",
                                    "Insufficient Permission",
                                )
                                .into_response()
                            }
                            _ => {}
                        }
                        REMOVED.lock().unwrap().push(q["id"].clone());
                        StatusCode::NO_CONTENT.into_response()
                    };
                    let comment = |h: HeaderMap,
                                   Query(q): Query<HashMap<String, String>>,
                                   Json(body): Json<Value>| async move {
                        if !bearer_ok(&h) {
                            return expired().into_response();
                        }
                        if COMMENT_MODE.load(Ordering::SeqCst) == 1 {
                            let msg = "The API server failed to successfully process the request.";
                            return google_error(StatusCode::BAD_REQUEST, "processingFailure", msg)
                                .into_response();
                        }
                        let bearer = h.get("authorization").and_then(|v| v.to_str().ok());
                        COMMENTS.lock().unwrap().push((
                            bearer.unwrap_or_default().to_owned(),
                            q["part"].clone(),
                            body,
                        ));
                        let n = NEXT.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            Json(json!({"kind": "youtube#commentThread",
                            "id": format!("Ugz{n}")})),
                        )
                            .into_response()
                    };
                    let thumbnail = |h: HeaderMap,
                                     Query(q): Query<HashMap<String, String>>,
                                     body: axum::body::Bytes| async move {
                        if !bearer_ok(&h) {
                            return expired().into_response();
                        }
                        if THUMB_MODE.load(Ordering::SeqCst) == 1 {
                            let msg = "The user doesn't have permissions to upload and set custom video thumbnails.";
                            return google_error(StatusCode::FORBIDDEN, "forbidden", msg)
                                .into_response();
                        }
                        let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok());
                        THUMBS.lock().unwrap().push((
                            get("authorization").unwrap_or_default().to_owned(),
                            q["videoId"].clone(),
                            get("content-type").unwrap_or_default().to_owned(),
                            body.len(),
                        ));
                        Json(json!({"kind": "youtube#thumbnailSetResponse"})).into_response()
                    };
                    let mut jpeg = vec![7u8; 64];
                    jpeg[..4].copy_from_slice(b"\xFF\xD8\xFF\xE0");
                    let mut mp4 = vec![0u8; 64];
                    mp4[..12].copy_from_slice(b"\0\0\0\x18ftypisom");
                    let app = Router::new()
                        .route("/token", post(token))
                        .route("/upload/youtube/v3/thumbnails/set", post(thumbnail))
                        .route(
                            "/cover.jpg",
                            get(move || async move { ([("content-type", "image/jpeg")], jpeg) }),
                        )
                        .route("/youtube/v3/channels", get(channels))
                        .route("/upload/youtube/v3/videos", post(start).put(upload))
                        .route("/youtube/v3/videos", delete(remove))
                        .route("/youtube/v3/commentThreads", post(comment))
                        .route("/img.png", get(|| async { image("image/png", 64) }))
                        .route(
                            "/clip.mp4",
                            get(move || async move { ([("content-type", "video/mp4")], mp4) }),
                        );
                    tokio::spawn(async move { axum::serve(listener, app).await });
                    base
                }

                /// A brain with YouTube set up on the mock, its state, and the mock.
                async fn on_youtube(
                    configured: bool,
                ) -> (TestServer, DatabaseConnection, AppState, String) {
                    let base = mock_youtube().await;
                    let mut config = Config::from_env().expect("test config");
                    config.trust_proxy = true;
                    let app = configured.then(|| ("yapp".to_owned(), "ysecret".to_owned()));
                    let mut y = YouTube::new(app).unwrap();
                    y.token_url = format!("{base}/token");
                    y.api_url = base.clone();
                    y.comment_wait = std::time::Duration::ZERO;
                    config.youtube = y;
                    let disk = object_store::local::LocalFileSystem::new_with_prefix(media_dir())
                        .unwrap()
                        .with_automatic_cleanup(true);
                    config.media = Media::new(std::sync::Arc::new(disk), true);
                    let db = socialreins_brain::connect(&config)
                        .await
                        .expect("database reachable");
                    let state = AppState::new(db.clone(), &config);
                    let app = controllers::router(state.clone(), &config);
                    (TestServer::new(app).unwrap(), db, state, base)
                }

                /// Starts a connect: (state, `sr_youtube=...`).
                async fn begin(s: &TestServer, cookie: &str) -> (String, String) {
                    let res = s
                        .get("/connections/youtube/redirect")
                        .add_header("cookie", cookie)
                        .await;
                    let (to, q) = location(&res);
                    assert!(
                        to.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"),
                        "{to}"
                    );
                    assert_eq!(q["client_id"], "yapp");
                    assert_eq!(q["response_type"], "code");
                    assert_eq!(
                        q["redirect_uri"],
                        "http://localhost:8080/connect/youtube/callback"
                    );
                    assert_eq!(q["access_type"], "offline");
                    assert_eq!(q["prompt"], "consent");
                    assert_eq!(q["include_granted_scopes"], "false");
                    assert_eq!(
                        q["scope"],
                        "https://www.googleapis.com/auth/youtube.upload \
                         https://www.googleapis.com/auth/youtube.readonly \
                         https://www.googleapis.com/auth/youtube.force-ssl"
                    );
                    let set = res
                        .iter_headers_by_name("set-cookie")
                        .map(|v| v.to_str().unwrap().to_owned())
                        .find(|c| c.starts_with("sr_youtube="))
                        .unwrap();
                    assert!(set.contains("HttpOnly") && set.contains("Path=/connect/youtube"));
                    (
                        q["state"].clone(),
                        set.split(';').next().unwrap().to_owned(),
                    )
                }

                /// Google's redirect back: where it lands, as `(connected, connect_error)`.
                async fn finish(
                    s: &TestServer,
                    cookies: &str,
                    query: &str,
                ) -> (Option<String>, Option<String>) {
                    let res = s
                        .get(&format!("/connect/youtube/callback?{query}"))
                        .add_header("cookie", cookies)
                        .await;
                    let (to, q) = location(&res);
                    assert!(
                        to.starts_with("http://localhost:3000/app/connections?"),
                        "{to}"
                    );
                    if q.contains_key("connect_error") {
                        assert_eq!(q["provider"], "youtube");
                    }
                    (q.get("connected").cloned(), q.get("connect_error").cloned())
                }

                /// Connects the mock's channel with `code` to the caller's current team.
                async fn connect_with(s: &TestServer, cookie: &str, code: &str) {
                    let (state, flow) = begin(s, cookie).await;
                    let both = format!("{cookie}; {flow}");
                    let got = finish(s, &both, &format!("code={code}&state={state}")).await;
                    assert_eq!(got, (Some("youtube".into()), None));
                }

                async fn youtube_account(
                    db: &DatabaseConnection,
                    team_id: &str,
                ) -> social_accounts::Model {
                    social_accounts::Entity::find()
                        .filter(
                            social_accounts::Column::TeamId
                                .eq(team_id.parse::<uuid::Uuid>().unwrap()),
                        )
                        .filter(social_accounts::Column::Provider.eq("youtube"))
                        .one(db)
                        .await
                        .unwrap()
                        .unwrap()
                }

                /// Sets the account's access token and the seconds it has left.
                async fn set_token(
                    db: &DatabaseConnection,
                    account: uuid::Uuid,
                    token: &str,
                    secs: i64,
                ) {
                    let row = social_accounts::ActiveModel {
                        id: Set(account),
                        access_token: Set(cipher().encrypt(token.as_bytes())),
                        token_expires_at: Set(Some(
                            (Utc::now() + Duration::seconds(secs)).fixed_offset(),
                        )),
                        ..Default::default()
                    };
                    social_accounts::Entity::update(row).exec(db).await.unwrap();
                }

                /// Sets the account's sealed refresh token, or removes it.
                async fn set_refresh(
                    db: &DatabaseConnection,
                    account: uuid::Uuid,
                    token: Option<&str>,
                ) {
                    let meta = match token {
                        Some(t) => {
                            json!({"refresh_token": URL_SAFE_NO_PAD.encode(cipher().encrypt(t.as_bytes()))})
                        }
                        None => json!({}),
                    };
                    let row = social_accounts::ActiveModel {
                        id: Set(account),
                        meta: Set(meta),
                        ..Default::default()
                    };
                    social_accounts::Entity::update(row).exec(db).await.unwrap();
                }

                fn youtube_post(set: &str, body: &str) -> Value {
                    let mut v = input(set, body, Some(&past()));
                    v["platform"] = json!("youtube");
                    v["media"] = json!(["https://a.example/clip.mp4"]);
                    v
                }

                /// A connected team: (server, db, state, mock base, cookie, team id, set id).
                async fn team() -> (
                    TestServer,
                    DatabaseConnection,
                    AppState,
                    String,
                    String,
                    String,
                    String,
                ) {
                    let (s, db, state, base) = on_youtube(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let team_id = current(&s, &cookie).await["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    connect_with(&s, &cookie, "good").await;
                    let set = default_set(&db, &team_id).await.to_string();
                    (s, db, state, base, cookie, team_id, set)
                }

                /// Saves a video post of the mock's clip and publishes it: the post.
                async fn publish_clip(
                    s: &TestServer,
                    (db, state, base): (&DatabaseConnection, &AppState, &str),
                    (cookie, set): (&str, &str),
                    body: &str,
                ) -> Value {
                    let id = id_of(&create(s, cookie, youtube_post(set, body)).await);
                    set_media(db, base, &id, &["/clip.mp4"]).await;
                    publish::tick(state).await.unwrap();
                    get_post(s, cookie, &id).await
                }

                #[tokio::test]
                #[serial]
                async fn connect_saves_the_channel_and_a_sealed_refresh_token() {
                    let (s, db, _, _) = on_youtube(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let team_id = current(&s, &cookie).await["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    let (state, flow) = begin(&s, &cookie).await;
                    let both = format!("{cookie}; {flow}");
                    let query = format!("code=good&state={state}");
                    let res = s
                        .get(&format!("/connect/youtube/callback?{query}"))
                        .add_header("cookie", &both)
                        .await;
                    let (_, q) = location(&res);
                    assert_eq!(q["connected"], "youtube");
                    // Only the connect cookie is cleared: nobody is signed in by this.
                    let cookies: Vec<String> = res
                        .iter_headers_by_name("set-cookie")
                        .map(|v| v.to_str().unwrap().to_owned())
                        .collect();
                    assert_eq!(cookies.len(), 1, "{cookies:?}");
                    assert!(cookies[0].starts_with("sr_youtube=;"), "{cookies:?}");

                    let listed = list(&s, &cookie).await;
                    assert_eq!(listed.len(), 1);
                    let a = &listed[0];
                    assert_eq!(a["provider"], "youtube");
                    assert_eq!(a["external_id"], "UCabc123");
                    assert_eq!(a["display_name"], "Ann Lee Makes");
                    assert_eq!(a["handle"], "annleemakes");
                    assert_eq!(a["avatar_url"], "https://yt3.ggpht.com/m=s240");
                    let set = default_set(&db, &team_id).await.to_string();
                    assert_eq!(a["set_ids"], json!([set]));
                    assert!(a.get("access_token").is_none() && !a.to_string().contains("yrefresh"));
                    let row = youtube_account(&db, &team_id).await;
                    assert_eq!(cipher().decrypt(&row.access_token).unwrap(), b"yt");
                    let sealed = row.meta["refresh_token"].as_str().unwrap();
                    assert!(!sealed.contains("yrefresh"));
                    let bytes = URL_SAFE_NO_PAD.decode(sealed).unwrap();
                    assert_eq!(cipher().decrypt(&bytes).unwrap(), b"yrefresh");
                    let left = row.token_expires_at.unwrap().timestamp() - Utc::now().timestamp();
                    assert!((3590..=3599).contains(&left), "{left}");
                    // The state works once.
                    assert_eq!(
                        finish(&s, &both, &query).await,
                        (None, Some("expired".into()))
                    );
                    // A reconnect keeps the one row.
                    connect_with(&s, &cookie, "good").await;
                    let again = list(&s, &cookie).await;
                    assert_eq!(again.len(), 1);
                    assert_eq!(again[0]["id"], a["id"]);
                }

                #[tokio::test]
                #[serial]
                async fn connect_refuses_strangers_members_and_other_browsers() {
                    let (s, db, _, _) = on_youtube(true).await;
                    let (_, cookie, user_id) = user(&s, &db).await;
                    let back = |res: axum_test::TestResponse| location(&res).1;
                    let res = s.get("/connections/youtube/redirect").await;
                    assert_eq!(back(res)["connect_error"], "unauthorized");

                    // The state must come back to the browser that started it.
                    let (state, flow) = begin(&s, &cookie).await;
                    let query = format!("code=good&state={state}");
                    assert_eq!(
                        finish(&s, &cookie, &query).await,
                        (None, Some("expired".into()))
                    );
                    let wrong = format!("{cookie}; sr_youtube=0000");
                    assert_eq!(
                        finish(&s, &wrong, &query).await,
                        (None, Some("expired".into()))
                    );
                    // Those did not use it up; another signed-in user cannot finish it.
                    let (_, stranger, _) = user(&s, &db).await;
                    let theirs = format!("{stranger}; {flow}");
                    assert_eq!(
                        finish(&s, &theirs, &query).await,
                        (None, Some("expired".into()))
                    );

                    // A demotion meanwhile.
                    let (state, flow) = begin(&s, &cookie).await;
                    set_role(&db, user_id, "member").await;
                    let both = format!("{cookie}; {flow}");
                    let q = format!("code=good&state={state}");
                    assert_eq!(finish(&s, &both, &q).await, (None, Some("expired".into())));
                    let res = s
                        .get("/connections/youtube/redirect")
                        .add_header("cookie", &cookie)
                        .await;
                    assert_eq!(back(res)["connect_error"], "forbidden");
                    assert!(list(&s, &cookie).await.is_empty());

                    let (s, db, _, _) = on_youtube(false).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let res = s
                        .get("/connections/youtube/redirect")
                        .add_header("cookie", &cookie)
                        .await;
                    assert_eq!(back(res)["connect_error"], "not_configured");
                }

                #[tokio::test]
                #[serial]
                async fn connects_that_go_wrong_land_with_a_code_and_save_nothing() {
                    let (s, db, _, _) = on_youtube(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    // Cancelled on Google, a bad code, no refresh token (the account could not be
                    // renewed), and a Google account without a YouTube channel.
                    for (q, want) in [
                        ("error=access_denied", "cancelled"),
                        ("code=bad", "youtube_failed"),
                        ("code=norefresh", "youtube_failed"),
                        ("code=nochannel", "no_channel"),
                    ] {
                        let (state, flow) = begin(&s, &cookie).await;
                        let both = format!("{cookie}; {flow}");
                        let got = finish(&s, &both, &format!("{q}&state={state}")).await;
                        assert_eq!(got, (None, Some(want.into())), "{q}");
                    }
                    assert!(list(&s, &cookie).await.is_empty());
                }

                #[tokio::test]
                #[serial]
                async fn a_consent_missing_a_permission_saves_nothing() {
                    let (s, db, _, _) = on_youtube(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    // Granular consent: a permission left unticked, or none named at all.
                    let bearers = BEARERS.lock().unwrap().len();
                    for code in ["noforcessl", "noupload", "noscope"] {
                        let (state, flow) = begin(&s, &cookie).await;
                        let both = format!("{cookie}; {flow}");
                        let got = finish(&s, &both, &format!("code={code}&state={state}")).await;
                        assert_eq!(got, (None, Some("missing_scopes".into())), "{code}");
                    }
                    assert!(list(&s, &cookie).await.is_empty());
                    // Not even the channel was looked up.
                    assert_eq!(BEARERS.lock().unwrap().len(), bearers);
                    // All three ticked connects.
                    connect_with(&s, &cookie, "good").await;
                    assert_eq!(list(&s, &cookie).await.len(), 1);
                }

                #[tokio::test]
                #[serial]
                async fn a_video_uploads_in_a_resumable_session_and_comes_down() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let (starts, puts) = (STARTS.lock().unwrap().len(), PUTS.lock().unwrap().len());
                    let body = "My first short\n\nSecond line of the description\nand a third";
                    let p = publish_clip(&s, (&db, &state, &base), (&cookie, &set), body).await;
                    assert_eq!(p["status"], "published", "{p}");

                    let (meta, length, kind) = STARTS.lock().unwrap()[starts].clone();
                    assert_eq!(meta["snippet"]["title"], "My first short");
                    assert_eq!(
                        meta["snippet"]["description"],
                        "Second line of the description\nand a third"
                    );
                    assert_eq!(meta["status"]["privacyStatus"], "public");
                    assert_eq!(meta["status"]["selfDeclaredMadeForKids"], false);
                    assert_eq!((length.as_str(), kind.as_str()), ("64", "video/mp4"));
                    let (size, sent) = PUTS.lock().unwrap()[puts].clone();
                    assert_eq!((size, sent.as_str()), (64, "Bearer yt"));

                    let t = &p["targets"][0];
                    let video = t["remote_id"].as_str().unwrap().to_owned();
                    assert!(video.starts_with("vid"), "{video}");
                    assert_eq!(
                        t["remote_url"],
                        format!("https://www.youtube.com/watch?v={video}")
                    );

                    let res = req(
                        &s,
                        Method::DELETE,
                        &format!("/v1/posts/{}", p["id"].as_str().unwrap()),
                        &cookie,
                    )
                    .await;
                    res.assert_status_ok();
                    assert_eq!(res.json::<Value>()["status"], "deleted");
                    assert_eq!(REMOVED.lock().unwrap().last(), Some(&video));
                }

                #[tokio::test]
                #[serial]
                async fn a_first_comment_goes_on_the_new_video_as_the_channel() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let mut post = youtube_post(&set, "Short\ndescribed");
                    post["first_comment"] = json!("Sources: example.org");
                    let id = id_of(&create(&s, &cookie, post).await);
                    set_media(&db, &base, &id, &["/clip.mp4"]).await;
                    let before = COMMENTS.lock().unwrap().len();
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "published", "{p}");
                    let t = &p["targets"][0];
                    let video = t["remote_id"].as_str().unwrap().to_owned();
                    let sent = COMMENTS.lock().unwrap()[before..].to_vec();
                    assert_eq!(sent.len(), 1);
                    assert_eq!(
                        (sent[0].0.as_str(), sent[0].1.as_str()),
                        ("Bearer yt", "snippet")
                    );
                    let snippet = &sent[0].2["snippet"];
                    assert_eq!(snippet["channelId"], "UCabc123");
                    assert_eq!(snippet["videoId"], video);
                    let text = &snippet["topLevelComment"]["snippet"]["textOriginal"];
                    assert_eq!(text, "Sources: example.org");
                    assert_eq!(t["comment_status"], "posted");
                    assert!(t["comment_id"].as_str().unwrap().starts_with("Ugz"));

                    // Taking the video down takes its comments: only the video is deleted.
                    let removed = REMOVED.lock().unwrap().len();
                    let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                    res.assert_status_ok();
                    assert_eq!(REMOVED.lock().unwrap()[removed..], [video]);
                }

                #[tokio::test]
                #[serial]
                async fn a_video_youtube_still_processes_fails_only_the_comment() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    COMMENT_MODE.store(1, Ordering::SeqCst);
                    let mut post = youtube_post(&set, "Short");
                    post["first_comment"] = json!("Sources");
                    let id = id_of(&create(&s, &cookie, post).await);
                    set_media(&db, &base, &id, &["/clip.mp4"]).await;
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(p["error"], Value::Null);
                    let t = &p["targets"][0];
                    assert_eq!(t["status"], "published");
                    assert_eq!(t["comment_status"], "failed");
                    let why = t["comment_error"].as_str().unwrap();
                    assert!(why.starts_with("Comment:"), "{why}");

                    // Not tried again, even when YouTube would take it now and the target is
                    // sent again (see the LinkedIn test of the guard).
                    COMMENT_MODE.store(0, Ordering::SeqCst);
                    requeue_unsent(&db, &id).await;
                    let (puts, comments) =
                        (PUTS.lock().unwrap().len(), COMMENTS.lock().unwrap().len());
                    publish::publish_post(&state, id.parse().unwrap())
                        .await
                        .unwrap();
                    assert_eq!(PUTS.lock().unwrap().len(), puts + 1, "uploaded again");
                    assert_eq!(COMMENTS.lock().unwrap().len(), comments);
                    assert_eq!(
                        get_post(&s, &cookie, &id).await["targets"][0]["comment_status"],
                        "failed"
                    );
                }

                /// Saves a video post with a cover of the mock and publishes it: the post.
                async fn publish_covered(
                    (s, db, state, base): (&TestServer, &DatabaseConnection, &AppState, &str),
                    (cookie, set): (&str, &str),
                    cover: &str,
                ) -> Value {
                    let mut post = youtube_post(set, "Short");
                    post["cover"] = json!("https://a.example/cover.jpg");
                    let id = id_of(&create(s, cookie, post).await);
                    set_media(db, base, &id, &["/clip.mp4"]).await;
                    set_cover_url(db, base, &id, cover).await;
                    publish::tick(state).await.unwrap();
                    get_post(s, cookie, &id).await
                }

                #[tokio::test]
                #[serial]
                async fn a_cover_is_set_on_the_video_once_it_is_up() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let ctx = (&s, &db, &state, base.as_str());
                    for (path, kind) in [("/cover.jpg", "image/jpeg"), ("/img.png", "image/png")] {
                        let before = THUMBS.lock().unwrap().len();
                        let p = publish_covered(ctx, (&cookie, &set), path).await;
                        assert_eq!(p["status"], "published", "{p}");
                        let t = &p["targets"][0];
                        assert_eq!(t["cover_error"], Value::Null);
                        let sent = THUMBS.lock().unwrap()[before..].to_vec();
                        let video = t["remote_id"].as_str().unwrap().to_owned();
                        assert_eq!(sent, [("Bearer yt".into(), video, kind.into(), 64)]);
                    }
                }

                #[tokio::test]
                #[serial]
                async fn a_channel_that_cannot_set_thumbnails_keeps_its_published_video() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    THUMB_MODE.store(1, Ordering::SeqCst);
                    let ctx = (&s, &db, &state, base.as_str());
                    let (starts, thumbs) =
                        (STARTS.lock().unwrap().len(), THUMBS.lock().unwrap().len());
                    let p = publish_covered(ctx, (&cookie, &set), "/cover.jpg").await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(p["error"], Value::Null);
                    assert_eq!(STARTS.lock().unwrap().len(), starts + 1, "uploaded once");
                    assert_eq!(THUMBS.lock().unwrap().len(), thumbs);
                    let t = &p["targets"][0];
                    assert_eq!(t["status"], "published");
                    assert!(t["remote_id"].is_string());
                    let why = t["cover_error"].as_str().unwrap();
                    assert!(why.starts_with("The video is up, but its cover image was not set."));
                    assert!(why.contains("verified"), "{why}");
                }

                #[tokio::test]
                #[serial]
                async fn a_cover_that_is_not_usable_fails_the_target_before_the_upload() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let ctx = (&s, &db, &state, base.as_str());
                    let (starts, thumbs) =
                        (STARTS.lock().unwrap().len(), THUMBS.lock().unwrap().len());
                    // A video where an image belongs, a page that is not there.
                    for (path, want) in [
                        ("/clip.mp4", "YouTube takes a JPEG or PNG cover image."),
                        (
                            "/nothing.jpg",
                            "The cover image could not be downloaded (HTTP 404).",
                        ),
                    ] {
                        let p = publish_covered(ctx, (&cookie, &set), path).await;
                        assert_eq!(p["status"], "failed", "{p}");
                        assert_eq!(p["targets"][0]["error"], want);
                        assert_eq!(p["targets"][0]["remote_id"], Value::Null);
                    }
                    assert_eq!(STARTS.lock().unwrap().len(), starts);
                    assert_eq!(THUMBS.lock().unwrap().len(), thumbs);
                }

                #[tokio::test]
                #[serial]
                async fn a_title_alone_is_enough_and_blank_lines_before_it_are_skipped() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let starts = STARTS.lock().unwrap().len();
                    let p = publish_clip(
                        &s,
                        (&db, &state, &base),
                        (&cookie, &set),
                        "\n  Just a title ",
                    )
                    .await;
                    assert_eq!(p["status"], "published", "{p}");
                    let (meta, _, _) = STARTS.lock().unwrap()[starts].clone();
                    assert_eq!(meta["snippet"]["title"], "Just a title");
                    assert_eq!(meta["snippet"]["description"], "");
                }

                #[tokio::test]
                #[serial]
                async fn posts_without_exactly_one_valid_video_are_refused() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let refused = |res: axum_test::TestResponse, code: &str| {
                        assert_eq!(res.status_code(), 422, "{}", res.text());
                        res.assert_json(&json!({"error": code}));
                    };
                    let mut none = youtube_post(&set, "Title");
                    none["media"] = json!([]);
                    refused(create(&s, &cookie, none).await, "media_required");
                    let mut two = youtube_post(&set, "Title");
                    two["media"] = json!(["https://a.example/1.mp4", "https://a.example/2.mp4"]);
                    refused(create(&s, &cookie, two).await, "too_many_images");
                    refused(
                        create(&s, &cookie, youtube_post(&set, &"a".repeat(101))).await,
                        "too_long",
                    );
                    let long = format!("Title\n{}", "a".repeat(5001));
                    refused(
                        create(&s, &cookie, youtube_post(&set, &long)).await,
                        "too_long",
                    );
                    refused(
                        create(&s, &cookie, youtube_post(&set, "")).await,
                        "empty_post",
                    );
                    refused(
                        create(&s, &cookie, youtube_post(&set, "a <b> c")).await,
                        "invalid_text",
                    );
                    let mut threaded = youtube_post(&set, "Title");
                    threaded["thread"] = json!([{"body": "two"}]);
                    refused(create(&s, &cookie, threaded).await, "thread_not_supported");
                    // The limits themselves pass.
                    let mut exact =
                        youtube_post(&set, &format!("{}\n{}", "a".repeat(100), "b".repeat(5000)));
                    exact["scheduled_at"] = Value::Null;
                    assert_eq!(create(&s, &cookie, exact).await.status_code(), 201);

                    // A file that is not an MP4 or MOV video never reaches YouTube.
                    let id = id_of(&create(&s, &cookie, youtube_post(&set, "Image")).await);
                    set_media(&db, &base, &id, &["/img.png"]).await;
                    let starts = STARTS.lock().unwrap().len();
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "failed");
                    let error = p["targets"][0]["error"].as_str().unwrap();
                    assert!(error.contains("not an MP4 or MOV video"), "{error}");
                    assert_eq!(STARTS.lock().unwrap().len(), starts);
                }

                #[tokio::test]
                #[serial]
                async fn foreign_session_urls_never_get_the_bytes_or_the_token() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    // A host that is not Google's: any request to it is counted.
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let port = listener.local_addr().unwrap().port();
                    let evil = format!("http://127.0.0.1:{port}");
                    let spy = Router::new().fallback(|| async {
                        FOREIGN_HITS.fetch_add(1, Ordering::SeqCst);
                        StatusCode::OK
                    });
                    tokio::spawn(async move { axum::serve(listener, spy).await });
                    let urls = [
                        format!("{evil}/x"),
                        format!("{base}@127.0.0.1:{port}/x"),
                        "https://www.googleapis.com.evil.net/x".to_owned(),
                        "https://www.googleapis.com@evil.net/x".to_owned(),
                        "https://evil.net/www.googleapis.com".to_owned(),
                        "http://www.googleapis.com/x".to_owned(),
                    ];
                    for url in &urls {
                        *FORCED_URL.lock().unwrap() = Some(url.clone());
                        let puts = PUTS.lock().unwrap().len();
                        let p = publish_clip(&s, (&db, &state, &base), (&cookie, &set), "x").await;
                        assert_eq!(p["status"], "failed", "{url}");
                        let error = p["targets"][0]["error"].as_str().unwrap();
                        assert!(error.contains("unexpected upload URL"), "{url}: {error}");
                        assert_eq!(PUTS.lock().unwrap().len(), puts, "{url}");
                    }
                    *FORCED_URL.lock().unwrap() = None;
                    assert_eq!(FOREIGN_HITS.load(Ordering::SeqCst), 0);
                }

                #[tokio::test]
                #[serial]
                async fn an_access_token_near_its_end_is_renewed_before_the_call() {
                    let (s, db, state, base, cookie, team_id, set) = team().await;
                    let account = youtube_account(&db, &team_id).await.id;
                    let ctx = (&db, &state, base.as_str());
                    // Half an hour left: used as it is.
                    set_token(&db, account, "yt", 1800).await;
                    let refreshes = REFRESHES.load(Ordering::SeqCst);
                    let p = publish_clip(&s, ctx, (&cookie, &set), "a").await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(REFRESHES.load(Ordering::SeqCst), refreshes);

                    // Within two minutes: renewed first, saved, and the new token is the one sent.
                    set_token(&db, account, "dead", 90).await;
                    let puts = PUTS.lock().unwrap().len();
                    let p = publish_clip(&s, ctx, (&cookie, &set), "b").await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(REFRESHES.load(Ordering::SeqCst), refreshes + 1);
                    assert_eq!(PUTS.lock().unwrap()[puts].1, "Bearer yrefreshed");
                    let row = youtube_account(&db, &team_id).await;
                    assert_eq!(cipher().decrypt(&row.access_token).unwrap(), b"yrefreshed");
                    let left = row.token_expires_at.unwrap().timestamp() - Utc::now().timestamp();
                    assert!((3590..=3599).contains(&left), "{left}");
                    // The refresh token stays.
                    let sealed = row.meta["refresh_token"].as_str().unwrap();
                    let bytes = URL_SAFE_NO_PAD.decode(sealed).unwrap();
                    assert_eq!(cipher().decrypt(&bytes).unwrap(), b"yrefresh");

                    // An expired one is renewed too, and a take down renews as well.
                    set_token(&db, account, "dead", -60).await;
                    let id = p["id"].as_str().unwrap().to_owned();
                    let video = p["targets"][0]["remote_id"].as_str().unwrap().to_owned();
                    let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                    res.assert_status_ok();
                    assert_eq!(REMOVED.lock().unwrap().last(), Some(&video));
                    assert_eq!(REFRESHES.load(Ordering::SeqCst), refreshes + 2);
                    assert_eq!(BEARERS.lock().unwrap().last().unwrap(), "Bearer yrefreshed");
                }

                #[tokio::test]
                #[serial]
                async fn a_refused_token_or_refresh_asks_for_a_reconnect() {
                    let (s, db, state, base, cookie, team_id, set) = team().await;
                    let account = youtube_account(&db, &team_id).await.id;
                    let ctx = (&db, &state, base.as_str());
                    let starts = STARTS.lock().unwrap().len();
                    // Revoked but not yet expired: Google answers 401.
                    set_token(&db, account, "dead", 1800).await;
                    let p = publish_clip(&s, ctx, (&cookie, &set), "a").await;
                    assert_eq!(p["status"], "failed");
                    assert_eq!(p["targets"][0]["error"], publish::YOUTUBE_RECONNECT);
                    // Expired, and Google refuses the refresh token.
                    set_token(&db, account, "dead", -60).await;
                    set_refresh(&db, account, Some("revoked")).await;
                    let p = publish_clip(&s, ctx, (&cookie, &set), "b").await;
                    assert_eq!(p["status"], "failed");
                    assert_eq!(p["targets"][0]["error"], publish::YOUTUBE_RECONNECT);
                    // Expired, with no refresh token at all: no call is made.
                    set_refresh(&db, account, None).await;
                    let refreshes = REFRESHES.load(Ordering::SeqCst);
                    let p = publish_clip(&s, ctx, (&cookie, &set), "c").await;
                    assert_eq!(p["status"], "failed");
                    assert_eq!(p["targets"][0]["error"], publish::YOUTUBE_RECONNECT);
                    assert_eq!(REFRESHES.load(Ordering::SeqCst), refreshes);
                    assert_eq!(STARTS.lock().unwrap().len(), starts);
                }

                #[tokio::test]
                #[serial]
                async fn an_exhausted_quota_is_named() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    QUOTA.store(true, Ordering::SeqCst);
                    let p = publish_clip(&s, (&db, &state, &base), (&cookie, &set), "a").await;
                    QUOTA.store(false, Ordering::SeqCst);
                    assert_eq!(p["status"], "failed", "{p}");
                    let error = p["targets"][0]["error"].as_str().unwrap();
                    assert!(error.contains("daily quota"), "{error}");
                    assert!(error.contains("1,600"), "{error}");
                }

                #[tokio::test]
                #[serial]
                async fn take_down_treats_a_gone_video_as_down_and_a_missing_scope_as_reconnect() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let ctx = (&db, &state, base.as_str());
                    let p = publish_clip(&s, ctx, (&cookie, &set), "a").await;
                    let path = format!("/v1/posts/{}", p["id"].as_str().unwrap());
                    // Connected before the delete scope existed: Google says the scope is missing.
                    DELETE_MODE.store(2, Ordering::SeqCst);
                    let res = req(&s, Method::DELETE, &path, &cookie).await;
                    DELETE_MODE.store(0, Ordering::SeqCst);
                    assert_eq!(res.status_code(), 422, "{}", res.text());
                    let body = res.json::<Value>();
                    assert_eq!(body["error"], "takedown_failed");
                    let msg = body["message"].as_str().unwrap();
                    assert!(
                        msg.contains("Reconnect the channel under Connections"),
                        "{msg}"
                    );
                    assert_eq!(
                        get_post(&s, &cookie, p["id"].as_str().unwrap()).await["status"],
                        "published"
                    );
                    // Deleted in YouTube Studio meanwhile: nothing left to take down.
                    DELETE_MODE.store(1, Ordering::SeqCst);
                    let res = req(&s, Method::DELETE, &path, &cookie).await;
                    DELETE_MODE.store(0, Ordering::SeqCst);
                    res.assert_status_ok();
                    assert_eq!(res.json::<Value>()["status"], "deleted");
                }
            }

            /// TikTok: the connect flow, direct posts of a video or photos that TikTok pulls from
            /// brain's links, against a mock of the Content Posting API; a post TikTok refuses,
            /// token renewal, and the missing delete.
            mod tiktok_api {
                use super::threads_api::{location, set_role};
                use super::*;
                use axum::{
                    extract::Form,
                    http::{HeaderMap, StatusCode},
                    routing::{get, post},
                    Json, Router,
                };
                use socialreins_brain::connectors::tiktok::TikTok;
                use std::sync::atomic::{AtomicU64, Ordering};

                /// Every init: (path, body).
                static INITS: std::sync::Mutex<Vec<(String, Value)>> =
                    std::sync::Mutex::new(Vec::new());
                /// The bearer token of every call that needs one, newest last.
                static BEARERS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
                static REFRESHES: AtomicU64 = AtomicU64::new(0);
                static STATUS_CALLS: AtomicU64 = AtomicU64::new(0);
                static NEXT: AtomicU64 = AtomicU64::new(1);
                /// 0: a public account, 1: only `SELF_ONLY` (an app TikTok has not reviewed).
                static PRIVACY: AtomicU64 = AtomicU64::new(0);
                /// 0: the init works, 1: TikTok refuses any level but `SELF_ONLY` as unaudited, 2: the publish
                /// fails, 3: TikTok refuses every init as unaudited.
                static MODE: AtomicU64 = AtomicU64::new(0);
                /// The next status call fails with a server error, once.
                static FLAKY: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);

                type Answer = (StatusCode, Json<Value>);

                fn ok(data: Value) -> Answer {
                    (
                        StatusCode::OK,
                        Json(json!({"data": data, "error": {"code": "ok", "message": ""}})),
                    )
                }

                fn refused(status: StatusCode, code: &str, message: &str) -> Answer {
                    let error = json!({"code": code, "message": message});
                    (status, Json(json!({"data": {}, "error": error})))
                }

                fn bearer_ok(h: &HeaderMap) -> bool {
                    let bearer = h.get("authorization").and_then(|v| v.to_str().ok());
                    BEARERS
                        .lock()
                        .unwrap()
                        .push(bearer.unwrap_or_default().to_owned());
                    matches!(bearer, Some("Bearer tt" | "Bearer ttrefreshed"))
                }

                fn expired() -> Answer {
                    let msg = "The access token is invalid or not authorized.";
                    refused(StatusCode::UNAUTHORIZED, "access_token_invalid", msg)
                }

                fn jpeg() -> ([(&'static str, &'static str); 1], Vec<u8>) {
                    let mut bytes = vec![7u8; 64];
                    bytes[..4].copy_from_slice(b"\xFF\xD8\xFF\xE0");
                    ([("content-type", "image/jpeg")], bytes)
                }

                /// TikTok (token endpoint, profile, creator info, init, status), plus a JPEG and an
                /// MP4. Code `good` gives a token with every scope, `noscope` none named,
                /// `nopublish` one without `video.publish`, `norefresh` no refresh token.
                async fn mock_tiktok() -> String {
                    PRIVACY.store(0, Ordering::SeqCst);
                    MODE.store(0, Ordering::SeqCst);
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let base = format!("http://{}", listener.local_addr().unwrap());
                    let token = |Form(f): Form<HashMap<String, String>>| async move {
                        let get = |k: &str| f.get(k).map(String::as_str).unwrap_or_default();
                        let app = get("client_key") == "kapp" && get("client_secret") == "ksecret";
                        let code = (get("grant_type"), get("code"), get("refresh_token"));
                        let all = "user.info.basic,video.upload,video.publish";
                        let bad = (
                            StatusCode::BAD_REQUEST,
                            Json(json!({"error": "invalid_grant",
                                "error_description": "Authorization code is expired."})),
                        );
                        let good = |scope: Option<&str>, refresh: Option<&str>| {
                            let mut v = json!({"access_token": "tt", "expires_in": 86400,
                                "open_id": "openid1", "token_type": "Bearer"});
                            if let Some(s) = scope {
                                v["scope"] = json!(s);
                            }
                            if let Some(r) = refresh {
                                v["refresh_token"] = json!(r);
                            }
                            (StatusCode::OK, Json(v))
                        };
                        match (app, code) {
                            (true, ("authorization_code", "good", _)) => {
                                good(Some(all), Some("krefresh"))
                            }
                            (true, ("authorization_code", "nopublish", _)) => {
                                good(Some("user.info.basic,video.upload"), Some("krefresh"))
                            }
                            (true, ("authorization_code", "noscope", _)) => {
                                good(None, Some("krefresh"))
                            }
                            (true, ("authorization_code", "norefresh", _)) => good(Some(all), None),
                            (true, ("refresh_token", _, "krefresh")) => {
                                REFRESHES.fetch_add(1, Ordering::SeqCst);
                                (
                                    StatusCode::OK,
                                    Json(json!({"access_token": "ttrefreshed",
                                        "expires_in": 86400, "refresh_token": "krefresh2"})),
                                )
                            }
                            _ => bad,
                        }
                    };
                    let user = |h: HeaderMap| async move {
                        if !bearer_ok(&h) {
                            return expired();
                        }
                        ok(
                            json!({"user": {"open_id": "openid1", "display_name": "Ann Lee",
                            "avatar_url": "https://p16.tiktokcdn.com/ann.jpeg"}}),
                        )
                    };
                    let creator = |h: HeaderMap| async move {
                        if !bearer_ok(&h) {
                            return expired();
                        }
                        let options = if PRIVACY.load(Ordering::SeqCst) == 1 {
                            json!(["SELF_ONLY"])
                        } else {
                            json!(["FOLLOWER_OF_CREATOR", "PUBLIC_TO_EVERYONE", "SELF_ONLY"])
                        };
                        ok(json!({"creator_username": "annlee", "privacy_level_options": options}))
                    };
                    let init = |path: &'static str| {
                        move |h: HeaderMap, Json(body): Json<Value>| async move {
                            if !bearer_ok(&h) {
                                return expired();
                            }
                            let level = body["post_info"]["privacy_level"]
                                .as_str()
                                .map(str::to_owned);
                            INITS.lock().unwrap().push((path.to_owned(), body));
                            // 1: refuses every level but SELF_ONLY, 3: refuses all.
                            let mode = MODE.load(Ordering::SeqCst);
                            if mode == 3 || (mode == 1 && level.as_deref() != Some("SELF_ONLY")) {
                                let code = "unaudited_client_can_only_post_to_private_accounts";
                                return refused(StatusCode::FORBIDDEN, code, "private only");
                            }
                            let n = NEXT.fetch_add(1, Ordering::SeqCst);
                            ok(json!({ "publish_id": format!("v_pub_url~v2.{n}") }))
                        }
                    };
                    let status = |h: HeaderMap, Json(body): Json<Value>| async move {
                        if !bearer_ok(&h) {
                            return expired();
                        }
                        assert!(body["publish_id"]
                            .as_str()
                            .unwrap()
                            .starts_with("v_pub_url~"));
                        if FLAKY.swap(false, Ordering::SeqCst) {
                            return refused(
                                StatusCode::INTERNAL_SERVER_ERROR,
                                "internal_error",
                                "",
                            );
                        }
                        // The first answer says TikTok is still downloading.
                        if STATUS_CALLS
                            .fetch_add(1, Ordering::SeqCst)
                            .is_multiple_of(2)
                        {
                            return ok(json!({"status": "PROCESSING_DOWNLOAD"}));
                        }
                        if MODE.load(Ordering::SeqCst) == 2 {
                            return ok(json!({"status": "FAILED",
                                "fail_reason": "file_format_check_failed"}));
                        }
                        if PRIVACY.load(Ordering::SeqCst) == 1 {
                            return ok(json!({"status": "PUBLISH_COMPLETE"}));
                        }
                        ok(json!({"status": "PUBLISH_COMPLETE",
                            "publicaly_available_post_id": [7311234567890123456_i64]}))
                    };
                    let mut mp4 = vec![0u8; 64];
                    mp4[..12].copy_from_slice(b"\0\0\0\x18ftypisom");
                    let app = Router::new()
                        .route("/v2/oauth/token/", post(token))
                        .route("/v2/user/info/", get(user))
                        .route("/v2/post/publish/creator_info/query/", post(creator))
                        .route("/v2/post/publish/video/init/", post(init("video")))
                        .route("/v2/post/publish/content/init/", post(init("content")))
                        .route("/v2/post/publish/status/fetch/", post(status))
                        .route("/a.jpg", get(|| async { jpeg() }))
                        .route("/b.jpg", get(|| async { jpeg() }))
                        .route("/img.png", get(|| async { image("image/png", 64) }))
                        .route(
                            "/clip.mp4",
                            get(move || async move { ([("content-type", "video/mp4")], mp4) }),
                        );
                    tokio::spawn(async move { axum::serve(listener, app).await });
                    base
                }

                /// A brain with TikTok set up on the mock, its state, and the mock.
                async fn on_tiktok(
                    configured: bool,
                ) -> (TestServer, DatabaseConnection, AppState, String) {
                    let base = mock_tiktok().await;
                    let mut config = Config::from_env().expect("test config");
                    config.trust_proxy = true;
                    let app = configured.then(|| ("kapp".to_owned(), "ksecret".to_owned()));
                    let mut t = TikTok::new(app).unwrap();
                    t.api_url = base.clone();
                    t.poll_every = std::time::Duration::from_millis(5);
                    config.tiktok = t;
                    let disk = object_store::local::LocalFileSystem::new_with_prefix(media_dir())
                        .unwrap()
                        .with_automatic_cleanup(true);
                    config.media = Media::new(std::sync::Arc::new(disk), true);
                    let db = socialreins_brain::connect(&config)
                        .await
                        .expect("database reachable");
                    let state = AppState::new(db.clone(), &config);
                    let app = controllers::router(state.clone(), &config);
                    (TestServer::new(app).unwrap(), db, state, base)
                }

                /// Starts a connect: (state, `sr_tiktok=...`).
                async fn begin(s: &TestServer, cookie: &str) -> (String, String) {
                    let res = s
                        .get("/connections/tiktok/redirect")
                        .add_header("cookie", cookie)
                        .await;
                    let (to, q) = location(&res);
                    assert!(
                        to.starts_with("https://www.tiktok.com/v2/auth/authorize/?"),
                        "{to}"
                    );
                    assert_eq!(q["client_key"], "kapp");
                    assert_eq!(q["response_type"], "code");
                    assert_eq!(
                        q["redirect_uri"],
                        "http://localhost:8080/connect/tiktok/callback"
                    );
                    assert_eq!(q["scope"], "user.info.basic,video.upload,video.publish");
                    let set = res
                        .iter_headers_by_name("set-cookie")
                        .map(|v| v.to_str().unwrap().to_owned())
                        .find(|c| c.starts_with("sr_tiktok="))
                        .unwrap();
                    assert!(set.contains("HttpOnly") && set.contains("Path=/connect/tiktok"));
                    (
                        q["state"].clone(),
                        set.split(';').next().unwrap().to_owned(),
                    )
                }

                /// TikTok's redirect back: where it lands, as `(connected, connect_error)`.
                async fn finish(
                    s: &TestServer,
                    cookies: &str,
                    query: &str,
                ) -> (Option<String>, Option<String>) {
                    let res = s
                        .get(&format!("/connect/tiktok/callback?{query}"))
                        .add_header("cookie", cookies)
                        .await;
                    let (to, q) = location(&res);
                    assert!(
                        to.starts_with("http://localhost:3000/app/connections?"),
                        "{to}"
                    );
                    if q.contains_key("connect_error") {
                        assert_eq!(q["provider"], "tiktok");
                    }
                    (q.get("connected").cloned(), q.get("connect_error").cloned())
                }

                async fn connect_with(s: &TestServer, cookie: &str, code: &str) {
                    let (state, flow) = begin(s, cookie).await;
                    let both = format!("{cookie}; {flow}");
                    let got = finish(s, &both, &format!("code={code}&state={state}")).await;
                    assert_eq!(got, (Some("tiktok".into()), None));
                }

                async fn tiktok_account(
                    db: &DatabaseConnection,
                    team_id: &str,
                ) -> social_accounts::Model {
                    social_accounts::Entity::find()
                        .filter(
                            social_accounts::Column::TeamId
                                .eq(team_id.parse::<uuid::Uuid>().unwrap()),
                        )
                        .filter(social_accounts::Column::Provider.eq("tiktok"))
                        .one(db)
                        .await
                        .unwrap()
                        .unwrap()
                }

                /// Sets the account's access token and the seconds it has left.
                async fn set_token(db: &DatabaseConnection, account: uuid::Uuid, secs: i64) {
                    let row = social_accounts::ActiveModel {
                        id: Set(account),
                        access_token: Set(cipher().encrypt(b"old")),
                        token_expires_at: Set(Some(
                            (Utc::now() + Duration::seconds(secs)).fixed_offset(),
                        )),
                        ..Default::default()
                    };
                    social_accounts::Entity::update(row).exec(db).await.unwrap();
                }

                fn tiktok_post(set: &str, body: &str, media: &[&str]) -> Value {
                    let mut v = input(set, body, Some(&past()));
                    v["platform"] = json!("tiktok");
                    v["media"] = json!(media
                        .iter()
                        .map(|m| format!("https://a.example{m}"))
                        .collect::<Vec<_>>());
                    v
                }

                /// A connected team: (server, db, state, mock base, cookie, team id, set id).
                async fn team() -> (
                    TestServer,
                    DatabaseConnection,
                    AppState,
                    String,
                    String,
                    String,
                    String,
                ) {
                    let (s, db, state, base) = on_tiktok(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let team_id = current(&s, &cookie).await["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    connect_with(&s, &cookie, "good").await;
                    let set = default_set(&db, &team_id).await.to_string();
                    (s, db, state, base, cookie, team_id, set)
                }

                /// Saves a post of the mock's files and publishes it: the post.
                async fn publish_files(
                    s: &TestServer,
                    (db, state, base): (&DatabaseConnection, &AppState, &str),
                    (cookie, set): (&str, &str),
                    (body, paths): (&str, &[&str]),
                ) -> Value {
                    let id = id_of(&create(s, cookie, tiktok_post(set, body, paths)).await);
                    set_media(db, base, &id, paths).await;
                    publish::tick(state).await.unwrap();
                    get_post(s, cookie, &id).await
                }

                #[tokio::test]
                #[serial]
                async fn connect_saves_the_account_and_a_sealed_refresh_token() {
                    let (s, db, _, _) = on_tiktok(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let team_id = current(&s, &cookie).await["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    let (state, flow) = begin(&s, &cookie).await;
                    let both = format!("{cookie}; {flow}");
                    let query = format!("code=good&state={state}");
                    let res = s
                        .get(&format!("/connect/tiktok/callback?{query}"))
                        .add_header("cookie", &both)
                        .await;
                    let (_, q) = location(&res);
                    assert_eq!(q["connected"], "tiktok");
                    // Only the connect cookie is cleared: nobody is signed in by this.
                    let cookies: Vec<String> = res
                        .iter_headers_by_name("set-cookie")
                        .map(|v| v.to_str().unwrap().to_owned())
                        .collect();
                    assert_eq!(cookies.len(), 1, "{cookies:?}");
                    assert!(cookies[0].starts_with("sr_tiktok=;"), "{cookies:?}");

                    let listed = list(&s, &cookie).await;
                    assert_eq!(listed.len(), 1);
                    let a = &listed[0];
                    assert_eq!(a["provider"], "tiktok");
                    assert_eq!(a["external_id"], "openid1");
                    assert_eq!(a["display_name"], "Ann Lee");
                    assert_eq!(a["handle"], "annlee");
                    assert_eq!(a["avatar_url"], "https://p16.tiktokcdn.com/ann.jpeg");
                    let set = default_set(&db, &team_id).await.to_string();
                    assert_eq!(a["set_ids"], json!([set]));
                    assert!(a.get("access_token").is_none() && !a.to_string().contains("krefresh"));
                    let row = tiktok_account(&db, &team_id).await;
                    assert_eq!(cipher().decrypt(&row.access_token).unwrap(), b"tt");
                    let sealed = row.meta["refresh_token"].as_str().unwrap();
                    assert!(!sealed.contains("krefresh"));
                    let plain = cipher()
                        .decrypt(&URL_SAFE_NO_PAD.decode(sealed).unwrap())
                        .unwrap();
                    assert_eq!(plain, b"krefresh");
                    // The state works once, and a reconnect keeps the one row.
                    assert_eq!(
                        finish(&s, &both, &query).await,
                        (None, Some("expired".into()))
                    );
                    connect_with(&s, &cookie, "good").await;
                    let again = list(&s, &cookie).await;
                    assert_eq!(again.len(), 1);
                    assert_eq!(again[0]["id"], a["id"]);
                }

                #[tokio::test]
                #[serial]
                async fn connect_refuses_strangers_members_and_other_browsers() {
                    let (s, db, _, _) = on_tiktok(true).await;
                    let (_, cookie, user_id) = user(&s, &db).await;
                    let back = |res: axum_test::TestResponse| location(&res).1;
                    let res = s.get("/connections/tiktok/redirect").await;
                    assert_eq!(back(res)["connect_error"], "unauthorized");

                    // The state must come back to the browser that started it.
                    let (state, _) = begin(&s, &cookie).await;
                    let query = format!("code=good&state={state}");
                    assert_eq!(
                        finish(&s, &cookie, &query).await,
                        (None, Some("expired".into()))
                    );
                    // A demotion meanwhile.
                    let (state, flow) = begin(&s, &cookie).await;
                    set_role(&db, user_id, "member").await;
                    let both = format!("{cookie}; {flow}");
                    let q = format!("code=good&state={state}");
                    assert_eq!(finish(&s, &both, &q).await, (None, Some("expired".into())));
                    let res = s
                        .get("/connections/tiktok/redirect")
                        .add_header("cookie", &cookie)
                        .await;
                    assert_eq!(back(res)["connect_error"], "forbidden");
                    assert!(list(&s, &cookie).await.is_empty());

                    let (s, db, _, _) = on_tiktok(false).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let res = s
                        .get("/connections/tiktok/redirect")
                        .add_header("cookie", &cookie)
                        .await;
                    assert_eq!(back(res)["connect_error"], "not_configured");
                }

                #[tokio::test]
                #[serial]
                async fn connects_that_go_wrong_land_with_a_code_and_save_nothing() {
                    let (s, db, _, _) = on_tiktok(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    // Cancelled on TikTok, a bad code, no refresh token (the account could not be
                    // renewed), and a permission left unticked or none named at all.
                    for (q, want) in [
                        ("error=access_denied", "cancelled"),
                        ("code=bad", "tiktok_failed"),
                        ("code=norefresh", "tiktok_failed"),
                        ("code=nopublish", "missing_scopes"),
                        ("code=noscope", "missing_scopes"),
                    ] {
                        let (state, flow) = begin(&s, &cookie).await;
                        let both = format!("{cookie}; {flow}");
                        let got = finish(&s, &both, &format!("{q}&state={state}")).await;
                        assert_eq!(got, (None, Some(want.into())), "{q}");
                    }
                    assert!(list(&s, &cookie).await.is_empty());
                }

                #[tokio::test]
                #[serial]
                async fn a_video_is_pulled_by_link_and_posted_publicly() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let (inits, polls) = (
                        INITS.lock().unwrap().len(),
                        STATUS_CALLS.load(Ordering::SeqCst),
                    );
                    let body = "  A caption for the clip \n#fyp";
                    let ctx = (&db, &state, base.as_str());
                    let p = publish_files(&s, ctx, (&cookie, &set), (body, &["/clip.mp4"])).await;
                    assert_eq!(p["status"], "published", "{p}");

                    let (path, sent) = INITS.lock().unwrap()[inits].clone();
                    assert_eq!(path, "video");
                    assert_eq!(sent["post_info"]["title"], "A caption for the clip \n#fyp");
                    assert_eq!(sent["post_info"]["privacy_level"], "PUBLIC_TO_EVERYONE");
                    for off in ["disable_duet", "disable_comment", "disable_stitch"] {
                        assert_eq!(sent["post_info"][off], true, "{off}");
                    }
                    assert_eq!(sent["source_info"]["source"], "PULL_FROM_URL");
                    let url = sent["source_info"]["video_url"].as_str().unwrap();
                    assert!(url.starts_with("http://localhost:8080/media/"), "{url}");
                    assert!(url.ends_with(".mp4"), "{url}");
                    // It asked again while TikTok was still downloading.
                    assert_eq!(STATUS_CALLS.load(Ordering::SeqCst) - polls, 2);

                    let t = &p["targets"][0];
                    assert!(t["remote_id"].as_str().unwrap().starts_with("v_pub_url~"));
                    assert_eq!(
                        t["remote_url"],
                        "https://www.tiktok.com/@annlee/video/7311234567890123456"
                    );
                }

                #[tokio::test]
                #[serial]
                async fn an_account_that_may_only_post_privately_gets_the_private_level() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    PRIVACY.store(1, Ordering::SeqCst);
                    let inits = INITS.lock().unwrap().len();
                    let ctx = (&db, &state, base.as_str());
                    let p = publish_files(&s, ctx, (&cookie, &set), ("hi", &["/clip.mp4"])).await;
                    assert_eq!(p["status"], "published", "{p}");
                    let (_, sent) = INITS.lock().unwrap()[inits].clone();
                    assert_eq!(sent["post_info"]["privacy_level"], "SELF_ONLY");
                    // A private post has no public video id, so no link.
                    assert_eq!(p["targets"][0]["remote_url"], Value::Null);
                }

                #[tokio::test]
                #[serial]
                async fn an_unreviewed_app_is_asked_again_with_the_private_level() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    MODE.store(1, Ordering::SeqCst);
                    let inits = INITS.lock().unwrap().len();
                    let ctx = (&db, &state, base.as_str());
                    let p = publish_files(&s, ctx, (&cookie, &set), ("hi", &["/clip.mp4"])).await;
                    assert_eq!(p["status"], "published", "{p}");
                    let sent = INITS.lock().unwrap()[inits..].to_vec();
                    let levels: Vec<&str> = sent
                        .iter()
                        .map(|(_, b)| b["post_info"]["privacy_level"].as_str().unwrap())
                        .collect();
                    assert_eq!(levels, ["PUBLIC_TO_EVERYONE", "SELF_ONLY"]);
                }

                #[tokio::test]
                #[serial]
                async fn photos_are_posted_as_a_photo_post() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let inits = INITS.lock().unwrap().len();
                    let ctx = (&db, &state, base.as_str());
                    let paths = ["/a.jpg", "/b.jpg"];
                    let p =
                        publish_files(&s, ctx, (&cookie, &set), ("Trip\nthe long caption", &paths))
                            .await;
                    assert_eq!(p["status"], "published", "{p}");
                    let (path, sent) = INITS.lock().unwrap()[inits].clone();
                    assert_eq!(path, "content");
                    assert_eq!(sent["media_type"], "PHOTO");
                    assert_eq!(sent["post_mode"], "DIRECT_POST");
                    assert_eq!(sent["post_info"]["title"], "Trip");
                    assert_eq!(sent["post_info"]["description"], "Trip\nthe long caption");
                    assert_eq!(sent["source_info"]["photo_cover_index"], 0);
                    let images = sent["source_info"]["photo_images"].as_array().unwrap();
                    assert_eq!(images.len(), 2);
                    for i in images {
                        let url = i.as_str().unwrap();
                        assert!(url.starts_with("http://localhost:8080/media/"), "{url}");
                    }
                }

                #[tokio::test]
                #[serial]
                async fn a_file_tiktok_cannot_take_fails_the_target_without_calling_tiktok() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let inits = INITS.lock().unwrap().len();
                    let ctx = (&db, &state, base.as_str());
                    // A PNG is not a TikTok photo.
                    let p = publish_files(&s, ctx, (&cookie, &set), ("a", &["/img.png"])).await;
                    assert_eq!(p["status"], "failed", "{p}");
                    let msg = p["targets"][0]["error"].as_str().unwrap();
                    assert!(
                        msg.contains("one MP4 or MOV video, or JPEG or WebP photos"),
                        "{msg}"
                    );
                    assert_eq!(INITS.lock().unwrap().len(), inits);
                }

                #[tokio::test]
                #[serial]
                async fn a_post_tiktok_refuses_or_fails_says_why() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let ctx = (&db, &state, base.as_str());
                    // The app is not reviewed and no level is taken.
                    MODE.store(3, Ordering::SeqCst);
                    let p = publish_files(&s, ctx, (&cookie, &set), ("a", &["/clip.mp4"])).await;
                    assert_eq!(p["status"], "failed", "{p}");
                    let msg = p["targets"][0]["error"].as_str().unwrap();
                    assert!(msg.contains("private account"), "{msg}");
                    // TikTok took the post and then could not publish it.
                    MODE.store(2, Ordering::SeqCst);
                    let p = publish_files(&s, ctx, (&cookie, &set), ("a", &["/clip.mp4"])).await;
                    assert_eq!(p["status"], "failed", "{p}");
                    let msg = p["targets"][0]["error"].as_str().unwrap();
                    assert!(msg.contains("file_format_check_failed"), "{msg}");
                }

                #[tokio::test]
                #[serial]
                async fn a_token_near_its_end_is_renewed_and_the_new_refresh_token_kept() {
                    let (s, db, state, base, cookie, team_id, set) = team().await;
                    let account = tiktok_account(&db, &team_id).await.id;
                    set_token(&db, account, 30).await;
                    let renewed = REFRESHES.load(Ordering::SeqCst);
                    let ctx = (&db, &state, base.as_str());
                    let p = publish_files(&s, ctx, (&cookie, &set), ("a", &["/clip.mp4"])).await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(REFRESHES.load(Ordering::SeqCst), renewed + 1);
                    assert_eq!(
                        BEARERS.lock().unwrap().last().map(String::as_str),
                        Some("Bearer ttrefreshed")
                    );
                    let row = tiktok_account(&db, &team_id).await;
                    assert_eq!(cipher().decrypt(&row.access_token).unwrap(), b"ttrefreshed");
                    let sealed = row.meta["refresh_token"].as_str().unwrap();
                    let plain = cipher()
                        .decrypt(&URL_SAFE_NO_PAD.decode(sealed).unwrap())
                        .unwrap();
                    assert_eq!(plain, b"krefresh2");
                    // A live token is not renewed again.
                    let p = publish_files(&s, ctx, (&cookie, &set), ("b", &["/clip.mp4"])).await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(REFRESHES.load(Ordering::SeqCst), renewed + 1);
                }

                #[tokio::test]
                #[serial]
                async fn two_renewals_at_once_spend_the_refresh_token_once() {
                    let (_, db, state, _, _, team_id, _) = team().await;
                    let row = tiktok_account(&db, &team_id).await;
                    set_token(&db, row.id, 30).await;
                    let renewed = REFRESHES.load(Ordering::SeqCst);
                    let fresh = |a: social_accounts::Model| {
                        let state = state.clone();
                        tokio::spawn(
                            async move { controllers::tiktok::fresh_token(&state, &a).await },
                        )
                    };
                    let (one, two) = (fresh(row.clone()), fresh(row));
                    let (one, two) = (one.await.unwrap(), two.await.unwrap());
                    assert_eq!(one.unwrap(), "ttrefreshed");
                    assert_eq!(two.unwrap(), "ttrefreshed");
                    // The mock refuses a second use of the old refresh token.
                    assert_eq!(REFRESHES.load(Ordering::SeqCst), renewed + 1);
                    let row = tiktok_account(&db, &team_id).await;
                    let sealed = row.meta["refresh_token"].as_str().unwrap();
                    let plain = cipher()
                        .decrypt(&URL_SAFE_NO_PAD.decode(sealed).unwrap())
                        .unwrap();
                    assert_eq!(plain, b"krefresh2");
                }

                #[tokio::test]
                #[serial]
                async fn a_status_call_that_fails_once_does_not_fail_the_post() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    FLAKY.store(true, Ordering::SeqCst);
                    let ctx = (&db, &state, base.as_str());
                    let p = publish_files(&s, ctx, (&cookie, &set), ("a", &["/clip.mp4"])).await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert!(!FLAKY.load(Ordering::SeqCst));
                }

                #[tokio::test]
                #[serial]
                async fn a_refresh_token_tiktok_refuses_asks_for_a_reconnect() {
                    let (s, db, state, base, cookie, team_id, set) = team().await;
                    let account = tiktok_account(&db, &team_id).await.id;
                    set_token(&db, account, 30).await;
                    let row = social_accounts::ActiveModel {
                        id: Set(account),
                        meta: Set(json!({"refresh_token":
                            URL_SAFE_NO_PAD.encode(cipher().encrypt(b"revoked"))})),
                        ..Default::default()
                    };
                    social_accounts::Entity::update(row)
                        .exec(&db)
                        .await
                        .unwrap();
                    let ctx = (&db, &state, base.as_str());
                    let p = publish_files(&s, ctx, (&cookie, &set), ("a", &["/clip.mp4"])).await;
                    assert_eq!(p["status"], "failed", "{p}");
                    let msg = p["targets"][0]["error"].as_str().unwrap();
                    assert!(
                        msg.contains("Reconnect the account under Connections"),
                        "{msg}"
                    );
                }

                #[tokio::test]
                #[serial]
                async fn take_down_says_tiktok_has_no_delete_and_keeps_the_post() {
                    let (s, db, state, base, cookie, _, set) = team().await;
                    let ctx = (&db, &state, base.as_str());
                    let p = publish_files(&s, ctx, (&cookie, &set), ("a", &["/clip.mp4"])).await;
                    let id = p["id"].as_str().unwrap();
                    let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                    assert_eq!(res.status_code(), 422, "{}", res.text());
                    let body = res.json::<Value>();
                    assert_eq!(body["error"], "takedown_failed");
                    let msg = body["message"].as_str().unwrap();
                    assert!(
                        msg.contains("does not let other apps delete posts"),
                        "{msg}"
                    );
                    assert!(msg.contains("Delete the post in the TikTok app"), "{msg}");
                    assert_eq!(get_post(&s, &cookie, id).await["status"], "published");
                }
            }

            /// X: the connect flow with PKCE, single-use refresh tokens, publishing text, media
            /// and threads against a mock of API v2, take down, and the failures X bills for.
            mod x_api {
                use super::threads_api::{location, set_role};
                use super::*;
                use axum::{
                    extract::{Form, Path, Query, State},
                    http::{HeaderMap, StatusCode},
                    response::{IntoResponse, Response},
                    routing::{delete, get, post},
                    Json, Router,
                };
                use base64::engine::general_purpose::STANDARD;
                use sea_orm::ConnectionTrait;
                use socialreins_brain::{connectors::x::X, controllers::x};
                use std::sync::{
                    atomic::{AtomicU64, Ordering},
                    Arc, Mutex,
                };

                const ALL_SCOPES: &str =
                    "tweet.read tweet.write users.read offline.access media.write";
                /// 2026-11-04 12:30:00 UTC, as `x-rate-limit-reset`.
                const RESET: &str = "1793795400";

                /// What the mock X saw and how it behaves.
                #[derive(Default)]
                struct Mock {
                    /// Refresh tokens that work once.
                    live: Mutex<Vec<String>>,
                    /// 0 as X does, 1 no new refresh token, 2 server error.
                    refresh_mode: AtomicU64,
                    refreshes: AtomicU64,
                    /// Each `code_verifier` and the grant it came with.
                    verifiers: Mutex<Vec<String>>,
                    /// Each initialize body.
                    inits: Mutex<Vec<Value>>,
                    /// Each append: (media id, segment index, body length).
                    appends: Mutex<Vec<(String, String, usize)>>,
                    categories: Mutex<HashMap<String, String>>,
                    polls: AtomicU64,
                    /// Every POST /2/tweets that reached the mock, and the bearer of each success.
                    attempts: AtomicU64,
                    tweets: Mutex<Vec<(String, Value)>>,
                    /// Every deleted tweet id.
                    removed: Mutex<Vec<String>>,
                    /// 0 deletes, 1 answers 404, 2 answers 429.
                    delete_mode: AtomicU64,
                    next: AtomicU64,
                    /// Access tokens X no longer accepts.
                    revoked: Mutex<Vec<String>>,
                    /// Simulated time passing: after this many more successful tweets or
                    /// deletes, the account's token has a minute left and the bearer that just
                    /// succeeded is no longer accepted.
                    clock: Mutex<Option<(DatabaseConnection, uuid::Uuid, usize)>>,
                }

                type Shared = State<Arc<Mock>>;

                /// Whether `who` may still call: not revoked.
                fn accepted(m: &Mock, who: &str) -> bool {
                    !m.revoked.lock().unwrap().iter().any(|t| t == who)
                }

                /// Counts one success against the clock; when it runs out, time "passes".
                async fn tick(m: &Mock, who: &str) {
                    let due = {
                        let mut clock = m.clock.lock().unwrap();
                        match clock.as_mut() {
                            Some((_, _, left)) if *left > 1 => {
                                *left -= 1;
                                None
                            }
                            Some(_) => clock.take(),
                            None => None,
                        }
                    };
                    if let Some((db, account, _)) = due {
                        m.revoked.lock().unwrap().push(who.to_owned());
                        db.execute_unprepared(&format!(
                            "UPDATE social_accounts SET token_expires_at = now() + \
                             interval '1 minute' WHERE id = '{account}'"
                        ))
                        .await
                        .unwrap();
                    }
                }

                fn bearer(h: &HeaderMap) -> Option<String> {
                    let v = h.get("authorization")?.to_str().ok()?;
                    let t = v.strip_prefix("Bearer ")?;
                    t.starts_with("xtok").then(|| t.to_owned())
                }

                fn dead() -> Response {
                    let body = json!({"title": "Unauthorized", "status": 401});
                    (StatusCode::UNAUTHORIZED, Json(body)).into_response()
                }

                async fn token(
                    State(m): Shared,
                    h: HeaderMap,
                    Form(f): Form<HashMap<String, String>>,
                ) -> Response {
                    let want = format!("Basic {}", STANDARD.encode("xapp:xsecret"));
                    let basic = h.get("authorization").and_then(|v| v.to_str().ok());
                    if basic != Some(want.as_str()) || f.contains_key("client_secret") {
                        return dead();
                    }
                    let refused = |why: &str| {
                        let body = json!({"error": "invalid_request", "error_description": why});
                        (StatusCode::BAD_REQUEST, Json(body)).into_response()
                    };
                    let get = |k: &str| f.get(k).map(String::as_str).unwrap_or_default();
                    match (get("grant_type"), get("code")) {
                        ("authorization_code", code @ ("good" | "narrow")) => {
                            m.verifiers
                                .lock()
                                .unwrap()
                                .push(get("code_verifier").to_owned());
                            let refresh = format!("xr-{}", uuid::Uuid::now_v7());
                            m.live.lock().unwrap().push(refresh.clone());
                            let scope = if code == "narrow" {
                                "tweet.read tweet.write users.read offline.access"
                            } else {
                                ALL_SCOPES
                            };
                            Json(json!({"token_type": "bearer", "access_token": "xtok",
                                "expires_in": 7200, "refresh_token": refresh, "scope": scope}))
                            .into_response()
                        }
                        ("refresh_token", _) => {
                            m.refreshes.fetch_add(1, Ordering::SeqCst);
                            if m.refresh_mode.load(Ordering::SeqCst) == 2 {
                                return StatusCode::SERVICE_UNAVAILABLE.into_response();
                            }
                            // Single use: the token is spent whatever happens next.
                            let spent = {
                                let mut live = m.live.lock().unwrap();
                                let at = live.iter().position(|t| t == get("refresh_token"));
                                at.map(|i| live.remove(i)).is_some()
                            };
                            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                            if !spent {
                                return refused("Value passed for the token was invalid.");
                            }
                            let n = m.next.fetch_add(1, Ordering::SeqCst);
                            let mut answer = json!({"token_type": "bearer",
                                "access_token": format!("xtok{n}"), "expires_in": 7200,
                                "scope": ALL_SCOPES});
                            if m.refresh_mode.load(Ordering::SeqCst) == 0 {
                                let refresh = format!("xr-{}", uuid::Uuid::now_v7());
                                m.live.lock().unwrap().push(refresh.clone());
                                answer["refresh_token"] = json!(refresh);
                            } else {
                                m.live.lock().unwrap().push(get("refresh_token").to_owned());
                            }
                            Json(answer).into_response()
                        }
                        _ => refused("The provided authorization code is invalid"),
                    }
                }

                async fn me(h: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
                    assert_eq!(q["user.fields"], "profile_image_url,username,name");
                    if bearer(&h).is_none() {
                        return dead();
                    }
                    Json(json!({"data": {"id": "2244994945", "username": "ann_lee",
                        "name": "Ann Lee", "profile_image_url": "https://pbs.example/p.jpg"}}))
                    .into_response()
                }

                async fn init(State(m): Shared, h: HeaderMap, Json(b): Json<Value>) -> Response {
                    if bearer(&h).is_none() {
                        return dead();
                    }
                    let id = (9000 + m.next.fetch_add(1, Ordering::SeqCst)).to_string();
                    let category = b["media_category"].as_str().unwrap().to_owned();
                    m.categories.lock().unwrap().insert(id.clone(), category);
                    m.inits.lock().unwrap().push(b);
                    Json(json!({"data": {"id": id, "media_key": format!("7_{id}"),
                        "expires_after_secs": 86400}}))
                    .into_response()
                }

                async fn append(
                    State(m): Shared,
                    Path(id): Path<String>,
                    h: HeaderMap,
                    body: axum::body::Bytes,
                ) -> Response {
                    if bearer(&h).is_none() {
                        return dead();
                    }
                    let kind = h.get("content-type").and_then(|v| v.to_str().ok());
                    assert!(kind.unwrap().starts_with("multipart/form-data; boundary="));
                    let text = String::from_utf8_lossy(&body);
                    let tag = "name=\"segment_index\"\r\n\r\n";
                    let at = text.find(tag).unwrap() + tag.len();
                    let index = text[at..].split("\r\n").next().unwrap().to_owned();
                    assert!(text.contains("name=\"media\""));
                    m.appends.lock().unwrap().push((id, index, body.len()));
                    StatusCode::NO_CONTENT.into_response()
                }

                async fn finalize(
                    State(m): Shared,
                    Path(id): Path<String>,
                    h: HeaderMap,
                ) -> Response {
                    if bearer(&h).is_none() {
                        return dead();
                    }
                    let video = m.categories.lock().unwrap().get(&id).map(String::as_str)
                        == Some("tweet_video");
                    let mut data = json!({"id": id, "media_key": format!("7_{id}"), "size": 1});
                    if video {
                        data["processing_info"] =
                            json!({"state": "pending", "check_after_secs": 0});
                    }
                    Json(json!({ "data": data })).into_response()
                }

                async fn status(
                    State(m): Shared,
                    h: HeaderMap,
                    Query(q): Query<HashMap<String, String>>,
                ) -> Response {
                    if bearer(&h).is_none() {
                        return dead();
                    }
                    assert_eq!(q["command"], "STATUS");
                    let state = match m.polls.fetch_add(1, Ordering::SeqCst) {
                        0 => "in_progress",
                        _ => "succeeded",
                    };
                    let info = json!({"state": state, "check_after_secs": 0});
                    Json(json!({"data": {"id": q["media_id"], "processing_info": info}}))
                        .into_response()
                }

                async fn tweet(State(m): Shared, h: HeaderMap, Json(b): Json<Value>) -> Response {
                    let Some(who) = bearer(&h).filter(|w| accepted(&m, w)) else {
                        return dead();
                    };
                    m.attempts.fetch_add(1, Ordering::SeqCst);
                    let text = b["text"].as_str().unwrap_or_default().to_owned();
                    if text.starts_with("CREDITS") {
                        let detail = "Your enrolled account [1] does not have any credits to \
                                      fulfill this request.";
                        let body = json!({"title": "CreditsDepleted", "detail": detail,
                            "type": "https://api.x.com/2/problems/credits"});
                        return (StatusCode::PAYMENT_REQUIRED, Json(body)).into_response();
                    }
                    if text.starts_with("LIMIT") {
                        let body = json!({"title": "Too Many Requests", "status": 429});
                        let headers = [("x-rate-limit-reset", RESET)];
                        return (StatusCode::TOO_MANY_REQUESTS, headers, Json(body))
                            .into_response();
                    }
                    if text.starts_with("REJECT") {
                        let detail = "You are not allowed to create a Tweet with duplicate \
                                      content. https://x.example/?access_token=abc";
                        let body = json!({"title": "Forbidden", "detail": detail});
                        return (StatusCode::FORBIDDEN, Json(body)).into_response();
                    }
                    for id in b["media"]["media_ids"].as_array().into_iter().flatten() {
                        assert!(m
                            .categories
                            .lock()
                            .unwrap()
                            .contains_key(id.as_str().unwrap()));
                    }
                    let id = (1_800_000_000_000_000_000u64 + m.next.fetch_add(1, Ordering::SeqCst))
                        .to_string();
                    m.tweets.lock().unwrap().push((who.clone(), b));
                    tick(&m, &who).await;
                    let body = json!({"data": {"id": id, "text": text}});
                    (StatusCode::CREATED, Json(body)).into_response()
                }

                async fn remove(
                    State(m): Shared,
                    Path(id): Path<String>,
                    h: HeaderMap,
                ) -> Response {
                    let Some(who) = bearer(&h).filter(|w| accepted(&m, w)) else {
                        return dead();
                    };
                    match m.delete_mode.load(Ordering::SeqCst) {
                        1 => (StatusCode::NOT_FOUND, Json(json!({"title": "Not Found"})))
                            .into_response(),
                        2 => {
                            let headers = [("x-rate-limit-reset", RESET)];
                            let body = json!({"title": "Too Many Requests"});
                            (StatusCode::TOO_MANY_REQUESTS, headers, Json(body)).into_response()
                        }
                        _ => {
                            m.removed.lock().unwrap().push(id);
                            tick(&m, &who).await;
                            Json(json!({"data": {"deleted": true}})).into_response()
                        }
                    }
                }

                /// X (OAuth, API v2, upload), plus images, a GIF and videos to post.
                async fn mock_x() -> (String, Arc<Mock>) {
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let base = format!("http://{}", listener.local_addr().unwrap());
                    let mock = Arc::new(Mock::default());
                    let video = |len: usize| async move {
                        let mut v = vec![0u8; len];
                        v[..14].copy_from_slice(b"\0\0\0\x18ftypmp42\0\0");
                        ([("content-type", "video/mp4")], v)
                    };
                    let gif = || async {
                        let mut v = vec![0u8; 40];
                        v[..12].copy_from_slice(b"GIF89a\x01\0\x01\0\0\0");
                        ([("content-type", "image/gif")], v)
                    };
                    let app = Router::new()
                        .route("/2/oauth2/token", post(token))
                        .route("/2/users/me", get(me))
                        .route("/2/media/upload/initialize", post(init))
                        .route("/2/media/upload/{id}/append", post(append))
                        .route("/2/media/upload/{id}/finalize", post(finalize))
                        .route("/2/media/upload", get(status))
                        .route("/2/tweets", post(tweet))
                        .route("/2/tweets/{id}", delete(remove))
                        .layer(axum::extract::DefaultBodyLimit::max(8_000_000))
                        .with_state(mock.clone())
                        .route("/img.png", get(|| async { image("image/png", 64) }))
                        .route("/img2.png", get(|| async { image("image/png", 48) }))
                        .route("/pic.gif", get(gif))
                        .route("/clip.mp4", get(move || video(64)))
                        .route("/big.mp4", get(move || video(4_000_001)));
                    tokio::spawn(async move { axum::serve(listener, app).await });
                    (base, mock)
                }

                /// A brain with X set up on the mock (no waits), its state, and the mock.
                async fn on_x(
                    configured: bool,
                ) -> (TestServer, DatabaseConnection, AppState, String, Arc<Mock>) {
                    let (base, mock) = mock_x().await;
                    let mut config = Config::from_env().expect("test config");
                    config.trust_proxy = true;
                    let app = configured.then(|| ("xapp".to_owned(), "xsecret".to_owned()));
                    let mut x = X::new(app).unwrap();
                    x.api_url = base.clone();
                    x.poll_every = std::time::Duration::ZERO;
                    config.x = x;
                    let disk = object_store::local::LocalFileSystem::new_with_prefix(media_dir())
                        .unwrap()
                        .with_automatic_cleanup(true);
                    config.media = Media::new(std::sync::Arc::new(disk), true);
                    let db = socialreins_brain::connect(&config)
                        .await
                        .expect("database reachable");
                    let state = AppState::new(db.clone(), &config);
                    let app = controllers::router(state.clone(), &config);
                    (TestServer::new(app).unwrap(), db, state, base, mock)
                }

                /// Starts a connect: (state, `sr_x=...`, the PKCE challenge sent to X).
                async fn begin(s: &TestServer, cookie: &str) -> (String, String, String) {
                    let res = s
                        .get("/connections/x/redirect")
                        .add_header("cookie", cookie)
                        .await;
                    let (to, q) = location(&res);
                    assert!(to.starts_with("https://x.com/i/oauth2/authorize?"), "{to}");
                    assert_eq!(q["client_id"], "xapp");
                    assert_eq!(q["response_type"], "code");
                    assert_eq!(
                        q["redirect_uri"],
                        "http://localhost:8080/connect/x/callback"
                    );
                    assert_eq!(q["scope"], ALL_SCOPES);
                    assert_eq!(q["code_challenge_method"], "S256");
                    assert!(!to.contains("xsecret") && !to.contains("verifier"));
                    let set = res
                        .iter_headers_by_name("set-cookie")
                        .map(|v| v.to_str().unwrap().to_owned())
                        .find(|c| c.starts_with("sr_x="))
                        .unwrap();
                    assert!(set.contains("HttpOnly") && set.contains("Path=/connect/x"));
                    (
                        q["state"].clone(),
                        set.split(';').next().unwrap().to_owned(),
                        q["code_challenge"].clone(),
                    )
                }

                /// X's redirect back: where it lands, as `(connected, connect_error)`.
                async fn finish(
                    s: &TestServer,
                    cookies: &str,
                    query: &str,
                ) -> (Option<String>, Option<String>) {
                    let res = s
                        .get(&format!("/connect/x/callback?{query}"))
                        .add_header("cookie", cookies)
                        .await;
                    let (to, q) = location(&res);
                    assert!(
                        to.starts_with("http://localhost:3000/app/connections?"),
                        "{to}"
                    );
                    if q.contains_key("connect_error") {
                        assert_eq!(q["provider"], "x");
                    }
                    (q.get("connected").cloned(), q.get("connect_error").cloned())
                }

                async fn connect_with(s: &TestServer, cookie: &str, code: &str) {
                    let (state, flow, _) = begin(s, cookie).await;
                    let both = format!("{cookie}; {flow}");
                    let got = finish(s, &both, &format!("code={code}&state={state}")).await;
                    assert_eq!(got, (Some("x".into()), None));
                }

                async fn x_account(
                    db: &DatabaseConnection,
                    team_id: &str,
                ) -> social_accounts::Model {
                    social_accounts::Entity::find()
                        .filter(
                            social_accounts::Column::TeamId
                                .eq(team_id.parse::<uuid::Uuid>().unwrap()),
                        )
                        .filter(social_accounts::Column::Provider.eq("x"))
                        .one(db)
                        .await
                        .unwrap()
                        .unwrap()
                }

                fn unseal(sealed: &str) -> String {
                    let bytes = URL_SAFE_NO_PAD.decode(sealed).unwrap();
                    String::from_utf8(cipher().decrypt(&bytes).unwrap()).unwrap()
                }

                fn seal(s: &str) -> String {
                    URL_SAFE_NO_PAD.encode(cipher().encrypt(s.as_bytes()))
                }

                /// The refresh token stored in the account's `meta`.
                fn refresh_of(a: &social_accounts::Model) -> String {
                    unseal(a.meta["refresh_token"].as_str().unwrap())
                }

                fn access_of(a: &social_accounts::Model) -> String {
                    String::from_utf8(cipher().decrypt(&a.access_token).unwrap()).unwrap()
                }

                /// Sets the account's access token and how many seconds it has left.
                async fn set_token(
                    db: &DatabaseConnection,
                    account: uuid::Uuid,
                    token: &str,
                    secs: i64,
                ) {
                    let row = social_accounts::ActiveModel {
                        id: Set(account),
                        access_token: Set(cipher().encrypt(token.as_bytes())),
                        token_expires_at: Set(Some(
                            (Utc::now() + Duration::seconds(secs)).fixed_offset(),
                        )),
                        ..Default::default()
                    };
                    social_accounts::Entity::update(row).exec(db).await.unwrap();
                }

                async fn set_meta(db: &DatabaseConnection, account: uuid::Uuid, meta: Value) {
                    let row = social_accounts::ActiveModel {
                        id: Set(account),
                        meta: Set(meta),
                        ..Default::default()
                    };
                    social_accounts::Entity::update(row).exec(db).await.unwrap();
                }

                /// Other tests left X accounts behind; the renewal job would renew those too.
                async fn forget_others(db: &DatabaseConnection, team_id: &str) {
                    db.execute_unprepared(&format!(
                        "DELETE FROM social_accounts WHERE provider = 'x' AND team_id <> '{team_id}'"
                    ))
                    .await
                    .unwrap();
                }

                fn x_post(set: &str, body: &str) -> Value {
                    let mut v = input(set, body, Some(&past()));
                    v["platform"] = json!("x");
                    v
                }

                fn thread(set: &str, body: &str, parts: &[&str]) -> Value {
                    let mut v = x_post(set, body);
                    v["thread"] = parts.iter().map(|p| json!({"body": p})).collect();
                    v
                }

                /// A connected team: (server, db, state, mock base, mock, cookie, team id, set id).
                async fn team() -> (
                    TestServer,
                    DatabaseConnection,
                    AppState,
                    String,
                    Arc<Mock>,
                    String,
                    String,
                    String,
                ) {
                    let (s, db, state, base, mock) = on_x(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let team_id = current(&s, &cookie).await["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    connect_with(&s, &cookie, "good").await;
                    let set = default_set(&db, &team_id).await.to_string();
                    (s, db, state, base, mock, cookie, team_id, set)
                }

                #[tokio::test]
                #[serial]
                async fn connect_uses_pkce_and_saves_the_profile_once_in_the_default_set() {
                    let (s, db, _, _, mock) = on_x(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let team_id = current(&s, &cookie).await["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    let (state, flow, challenge) = begin(&s, &cookie).await;

                    // The verifier waits in the database, sealed, and matches the challenge.
                    let row = db
                        .query_one_raw(sea_orm::Statement::from_sql_and_values(
                            sea_orm::DbBackend::Postgres,
                            "SELECT verifier FROM oauth_states WHERE state_hash = $1",
                            [socialreins_core::token::hash(&state).into()],
                        ))
                        .await
                        .unwrap()
                        .unwrap();
                    let stored: String = row.try_get("", "verifier").unwrap();
                    let verifier = unseal(&stored);
                    assert_ne!(stored, verifier);
                    assert_eq!(
                        socialreins_core::oauth::pkce_challenge(&verifier),
                        challenge
                    );

                    let both = format!("{cookie}; {flow}");
                    let query = format!("code=good&state={state}");
                    assert_eq!(finish(&s, &both, &query).await, (Some("x".into()), None));
                    // X was given the verifier, with the client secret in Basic auth only.
                    assert_eq!(mock.verifiers.lock().unwrap().clone(), [verifier]);

                    let listed = list(&s, &cookie).await;
                    assert_eq!(listed.len(), 1);
                    let a = &listed[0];
                    assert_eq!(a["provider"], "x");
                    assert_eq!(a["external_id"], "2244994945");
                    assert_eq!(a["handle"], "ann_lee");
                    assert_eq!(a["display_name"], "Ann Lee");
                    assert_eq!(a["avatar_url"], "https://pbs.example/p.jpg");
                    let set = default_set(&db, &team_id).await.to_string();
                    assert_eq!(a["set_ids"], json!([set]));
                    let row = x_account(&db, &team_id).await;
                    assert_eq!(access_of(&row), "xtok");
                    assert!(refresh_of(&row).starts_with("xr-"));
                    assert!(!row.meta.to_string().contains("xr-"), "sealed at rest");
                    let left = row.token_expires_at.unwrap().timestamp() - Utc::now().timestamp();
                    assert!((7100..=7200).contains(&left), "{left}");

                    // The state works once.
                    assert_eq!(
                        finish(&s, &both, &query).await,
                        (None, Some("expired".into()))
                    );
                    // A reconnect keeps the one row and clears the reconnect flag.
                    set_meta(&db, row.id, json!({"reconnect": true})).await;
                    connect_with(&s, &cookie, "good").await;
                    let again = list(&s, &cookie).await;
                    assert_eq!(again.len(), 1);
                    assert_eq!(again[0]["id"], a["id"]);
                    let row = x_account(&db, &team_id).await;
                    assert!(row.meta.get("reconnect").is_none());
                    assert!(refresh_of(&row).starts_with("xr-"));
                }

                #[tokio::test]
                #[serial]
                async fn connect_refuses_strangers_members_other_browsers_and_missing_scopes() {
                    let (s, db, _, _, _) = on_x(true).await;
                    let (_, cookie, user_id) = user(&s, &db).await;
                    let back = |res: axum_test::TestResponse| location(&res).1;
                    let res = s.get("/connections/x/redirect").await;
                    assert_eq!(back(res)["connect_error"], "unauthorized");

                    // The state must come back to the browser that started it.
                    let (state, flow, _) = begin(&s, &cookie).await;
                    let query = format!("code=good&state={state}");
                    assert_eq!(
                        finish(&s, &cookie, &query).await,
                        (None, Some("expired".into()))
                    );
                    let wrong = format!("{cookie}; sr_x=0000");
                    assert_eq!(
                        finish(&s, &wrong, &query).await,
                        (None, Some("expired".into()))
                    );
                    // Those did not use it up; another signed-in user cannot finish it.
                    let (_, stranger, _) = user(&s, &db).await;
                    let theirs = format!("{stranger}; {flow}");
                    assert_eq!(
                        finish(&s, &theirs, &query).await,
                        (None, Some("expired".into()))
                    );

                    // Cancelled on X, a bad code, a narrowed consent, and a demotion meanwhile.
                    let (state, flow, _) = begin(&s, &cookie).await;
                    let both = format!("{cookie}; {flow}");
                    let q = format!("error=access_denied&state={state}");
                    assert_eq!(
                        finish(&s, &both, &q).await,
                        (None, Some("cancelled".into()))
                    );
                    let (state, flow, _) = begin(&s, &cookie).await;
                    let both = format!("{cookie}; {flow}");
                    let q = format!("code=bad&state={state}");
                    assert_eq!(finish(&s, &both, &q).await, (None, Some("x_failed".into())));
                    let (state, flow, _) = begin(&s, &cookie).await;
                    let both = format!("{cookie}; {flow}");
                    let q = format!("code=narrow&state={state}");
                    assert_eq!(
                        finish(&s, &both, &q).await,
                        (None, Some("missing_scopes".into()))
                    );
                    let (state, flow, _) = begin(&s, &cookie).await;
                    set_role(&db, user_id, "member").await;
                    let both = format!("{cookie}; {flow}");
                    let q = format!("code=good&state={state}");
                    assert_eq!(finish(&s, &both, &q).await, (None, Some("expired".into())));
                    let res = s
                        .get("/connections/x/redirect")
                        .add_header("cookie", &cookie)
                        .await;
                    assert_eq!(back(res)["connect_error"], "forbidden");
                    assert!(list(&s, &cookie).await.is_empty());

                    let (s, db, _, _, _) = on_x(false).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let res = s
                        .get("/connections/x/redirect")
                        .add_header("cookie", &cookie)
                        .await;
                    assert_eq!(back(res)["connect_error"], "not_configured");
                    let res = s.get("/connect/x/callback?code=good&state=s").await;
                    assert_eq!(location(&res).1["connect_error"], "not_configured");
                }

                #[tokio::test]
                #[serial]
                async fn a_text_post_publishes_with_its_permalink_and_comes_down() {
                    let (s, _, state, _, mock, cookie, _, set) = team().await;
                    let id = id_of(&create(&s, &cookie, x_post(&set, "Hello X")).await);
                    let before = mock.tweets.lock().unwrap().len();
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "published", "{p}");
                    let (who, sent) = mock.tweets.lock().unwrap()[before].clone();
                    assert_eq!(who, "xtok");
                    assert_eq!(sent, json!({"text": "Hello X"}));
                    let t = &p["targets"][0];
                    let tweet = t["remote_id"].as_str().unwrap().to_owned();
                    assert_eq!(
                        t["remote_url"],
                        format!("https://x.com/ann_lee/status/{tweet}")
                    );

                    let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                    res.assert_status_ok();
                    assert_eq!(res.json::<Value>()["status"], "deleted");
                    assert_eq!(mock.removed.lock().unwrap().last(), Some(&tweet));
                }

                #[tokio::test]
                #[serial]
                async fn images_a_gif_and_a_video_upload_first_and_the_post_names_them() {
                    let (s, db, state, base, mock, cookie, _, set) = team().await;
                    let id = id_of(&create(&s, &cookie, x_post(&set, "two")).await);
                    set_media(&db, &base, &id, &["/img.png", "/img2.png"]).await;
                    let (tweets, inits) = (
                        mock.tweets.lock().unwrap().len(),
                        mock.inits.lock().unwrap().len(),
                    );
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "published", "{p}");
                    let started = mock.inits.lock().unwrap()[inits..].to_vec();
                    assert_eq!(started.len(), 2);
                    assert_eq!(
                        started[0],
                        json!({"media_type": "image/png", "total_bytes": 64,
                            "media_category": "tweet_image"})
                    );
                    assert_eq!(started[1]["total_bytes"], 48);
                    let sent = mock.tweets.lock().unwrap()[tweets].1.clone();
                    let ids = sent["media"]["media_ids"].as_array().unwrap();
                    assert_eq!(ids.len(), 2);
                    assert_ne!(ids[0], ids[1]);
                    let appends = mock.appends.lock().unwrap().clone();
                    let mine: Vec<_> = appends
                        .iter()
                        .filter(|a| ids.contains(&json!(a.0)))
                        .collect();
                    assert_eq!(mine.len(), 2);
                    assert!(mine.iter().all(|a| a.1 == "0"));

                    // A GIF is its own category.
                    let id = id_of(&create(&s, &cookie, x_post(&set, "gif")).await);
                    set_media(&db, &base, &id, &["/pic.gif"]).await;
                    let inits = mock.inits.lock().unwrap().len();
                    publish::tick(&state).await.unwrap();
                    assert_eq!(get_post(&s, &cookie, &id).await["status"], "published");
                    let started = mock.inits.lock().unwrap()[inits..].to_vec();
                    assert_eq!(started[0]["media_category"], "tweet_gif");
                    assert_eq!(started[0]["media_type"], "image/gif");

                    // A video over one chunk goes in segments 0 and 1, and is polled until X is done.
                    let id = id_of(&create(&s, &cookie, x_post(&set, "clip")).await);
                    set_media(&db, &base, &id, &["/big.mp4"]).await;
                    let (inits, appends, polls) = (
                        mock.inits.lock().unwrap().len(),
                        mock.appends.lock().unwrap().len(),
                        mock.polls.load(Ordering::SeqCst),
                    );
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "published", "{p}");
                    let started = mock.inits.lock().unwrap()[inits..].to_vec();
                    assert_eq!(started[0]["media_category"], "tweet_video");
                    assert_eq!(started[0]["total_bytes"], 4_000_001);
                    let parts = mock.appends.lock().unwrap()[appends..].to_vec();
                    let indexes: Vec<&str> = parts.iter().map(|a| a.1.as_str()).collect();
                    assert_eq!(indexes, ["0", "1"]);
                    assert!(parts[0].2 > 4_000_000 && parts[1].2 < 1_000);
                    assert_eq!(mock.polls.load(Ordering::SeqCst), polls + 2);
                }

                #[tokio::test]
                #[serial]
                async fn limits_and_unsupported_shapes_are_refused() {
                    let (s, db, state, base, mock, cookie, _, set) = team().await;
                    // 280 weighted characters, links counted as 23.
                    let res = create(&s, &cookie, x_post(&set, &"a".repeat(281))).await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json(&json!({"error": "too_long"}));
                    let link =
                        format!("{} https://example.com/{}", "a".repeat(256), "b".repeat(90));
                    let mut ok = x_post(&set, &link);
                    ok["scheduled_at"] = Value::Null;
                    assert_eq!(create(&s, &cookie, ok).await.status_code(), 201);
                    let mut many = x_post(&set, "x");
                    many["media"] = json!(vec!["https://a.example/i.png"; 5]);
                    let res = create(&s, &cookie, many).await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json(&json!({"error": "too_many_images"}));
                    let res =
                        create(&s, &cookie, thread(&set, "x", &["a".repeat(281).as_str()])).await;
                    assert_eq!(res.status_code(), 422);
                    assert_eq!(res.json::<Value>()["part"], 1);

                    // A video next to an image, or a GIF next to an image, never reaches X.
                    for second in ["/clip.mp4", "/pic.gif"] {
                        let id = id_of(&create(&s, &cookie, x_post(&set, "mix")).await);
                        set_media(&db, &base, &id, &["/img.png", second]).await;
                        let inits = mock.inits.lock().unwrap().len();
                        publish::tick(&state).await.unwrap();
                        let p = get_post(&s, &cookie, &id).await;
                        assert_eq!(p["status"], "failed", "{second}");
                        assert!(p["targets"][0]["error"]
                            .as_str()
                            .unwrap()
                            .contains("not a mix"));
                        assert_eq!(mock.inits.lock().unwrap().len(), inits);
                    }
                }

                #[tokio::test]
                #[serial]
                async fn a_thread_goes_out_as_replies_and_comes_down_children_first() {
                    let (s, _, state, _, mock, cookie, _, set) = team().await;
                    let body = thread(&set, "one", &["two", "three"]);
                    let id = id_of(&create(&s, &cookie, body).await);
                    let before = mock.tweets.lock().unwrap().len();
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "published", "{p}");
                    let t = &p["targets"][0];
                    let root = t["remote_id"].as_str().unwrap().to_owned();
                    let parts = t["parts"].as_array().unwrap();
                    assert_eq!(parts.len(), 2);
                    let (first, second) = (
                        parts[0]["remote_id"].as_str().unwrap().to_owned(),
                        parts[1]["remote_id"].as_str().unwrap().to_owned(),
                    );
                    assert_eq!(
                        parts[1]["remote_url"],
                        format!("https://x.com/ann_lee/status/{second}")
                    );
                    let sent: Vec<Value> = mock.tweets.lock().unwrap()[before..]
                        .iter()
                        .map(|t| t.1.clone())
                        .collect();
                    assert_eq!(sent[0], json!({"text": "one"}));
                    assert_eq!(
                        sent[1],
                        json!({"text": "two", "reply": {"in_reply_to_tweet_id": root}})
                    );
                    assert_eq!(
                        sent[2],
                        json!({"text": "three", "reply": {"in_reply_to_tweet_id": first}})
                    );

                    let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                    res.assert_status_ok();
                    let body = res.json::<Value>();
                    assert_eq!(body["status"], "deleted");
                    assert_eq!(body["targets"][0]["parts"], json!([]));
                    let removed = mock.removed.lock().unwrap().clone();
                    assert_eq!(removed[removed.len() - 3..], [second, first, root]);
                }

                #[tokio::test]
                #[serial]
                async fn a_token_that_runs_out_during_a_thread_is_renewed_before_the_next_part() {
                    let (s, db, state, _, mock, cookie, team_id, set) = team().await;
                    let account = x_account(&db, &team_id).await;
                    // 20 minutes left: no renewal at the start. After the first tweet time
                    // "passes": the token has a minute left and X no longer takes it.
                    set_token(&db, account.id, "xtok", 20 * 60).await;
                    *mock.clock.lock().unwrap() = Some((db.clone(), account.id, 1));
                    let body = thread(&set, "one", &["two", "three"]);
                    let id = id_of(&create(&s, &cookie, body).await);
                    let before = mock.tweets.lock().unwrap().len();
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "published", "{p}");
                    let t = &p["targets"][0];
                    assert_eq!(t["status"], "published", "{t}");
                    assert_eq!(t["parts"].as_array().unwrap().len(), 2);
                    assert_eq!(mock.refreshes.load(Ordering::SeqCst), 1, "renewed once");
                    let sent: Vec<String> = mock.tweets.lock().unwrap()[before..]
                        .iter()
                        .map(|t| t.0.clone())
                        .collect();
                    assert_eq!(sent.len(), 3);
                    assert_eq!(sent[0], "xtok");
                    let row = x_account(&db, &team_id).await;
                    assert_eq!(sent[1], access_of(&row), "part two used the renewed token");
                    assert_eq!(sent[2], sent[1]);
                    assert!(row.meta.get("reconnect").is_none());
                }

                #[tokio::test]
                #[serial]
                async fn a_take_down_renews_the_token_between_deletes() {
                    let (s, db, state, _, mock, cookie, team_id, set) = team().await;
                    let id =
                        id_of(&create(&s, &cookie, thread(&set, "one", &["two", "three"])).await);
                    publish::tick(&state).await.unwrap();
                    assert_eq!(get_post(&s, &cookie, &id).await["status"], "published");
                    // Time passes after the first delete.
                    let account = x_account(&db, &team_id).await;
                    set_token(&db, account.id, "xtok", 20 * 60).await;
                    *mock.clock.lock().unwrap() = Some((db.clone(), account.id, 1));
                    let removed = mock.removed.lock().unwrap().len();
                    let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                    res.assert_status_ok();
                    assert_eq!(res.json::<Value>()["status"], "deleted");
                    assert_eq!(mock.removed.lock().unwrap().len(), removed + 3);
                    assert_eq!(mock.refreshes.load(Ordering::SeqCst), 1);
                }

                #[tokio::test]
                #[serial]
                async fn a_failed_part_keeps_the_parts_before_it_and_is_never_retried() {
                    let (s, db, state, _, mock, cookie, _, set) = team().await;
                    let body = thread(&set, "one", &["two", "REJECT three", "four"]);
                    let id = id_of(&create(&s, &cookie, body).await);
                    let (tweets, attempts) = (
                        mock.tweets.lock().unwrap().len(),
                        mock.attempts.load(Ordering::SeqCst),
                    );
                    publish::tick(&state).await.unwrap();
                    assert_eq!(mock.tweets.lock().unwrap().len(), tweets + 2);
                    assert_eq!(
                        mock.attempts.load(Ordering::SeqCst),
                        attempts + 3,
                        "four never went"
                    );
                    let p = get_post(&s, &cookie, &id).await;
                    // The root is up, so the post is published and can come down.
                    assert_eq!(p["status"], "published", "{p}");
                    let t = &p["targets"][0];
                    assert_eq!(t["status"], "failed");
                    assert_eq!(
                        t["error"],
                        "Thread post 2 of 3: Publish: You are not allowed to create a Tweet \
                         with duplicate content. The posts before it are up."
                    );
                    assert_eq!(t["parts"].as_array().unwrap().len(), 1);
                    assert!(t["remote_url"].is_string());

                    // Never sent again.
                    publish::publish_post(&state, id.parse().unwrap())
                        .await
                        .unwrap();
                    publish::recover_stuck(&db).await.unwrap();
                    assert_eq!(mock.attempts.load(Ordering::SeqCst), attempts + 3);

                    let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                    res.assert_status_ok();
                    let body = res.json::<Value>();
                    assert_eq!(body["status"], "deleted");
                    assert_eq!(body["targets"][0]["status"], "deleted");
                    let removed = mock.removed.lock().unwrap().clone();
                    assert_eq!(removed.len(), 2, "the saved part and the root");
                }

                #[tokio::test]
                #[serial]
                async fn out_of_credits_and_rate_limits_say_so_and_are_not_retried() {
                    let (s, _, state, _, mock, cookie, _, set) = team().await;
                    let attempts = mock.attempts.load(Ordering::SeqCst);
                    let id = id_of(&create(&s, &cookie, x_post(&set, "CREDITS please")).await);
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "failed");
                    let why = p["targets"][0]["error"].as_str().unwrap();
                    assert!(why.contains("out of API credits"), "{why}");
                    assert!(why.contains("Add credits"), "{why}");
                    assert_eq!(mock.attempts.load(Ordering::SeqCst), attempts + 1);

                    let id = id_of(&create(&s, &cookie, x_post(&set, "LIMIT me")).await);
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "failed");
                    assert_eq!(
                        p["targets"][0]["error"],
                        "Publish: X rate limit reached; it resets at 2026-11-04 12:30 UTC. It \
                         was not retried."
                    );
                    assert_eq!(mock.attempts.load(Ordering::SeqCst), attempts + 2);

                    // Credits run out midway through a thread: the posts before it stay up.
                    let body = thread(&set, "one", &["CREDITS two"]);
                    let id = id_of(&create(&s, &cookie, body).await);
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "published", "{p}");
                    let why = p["targets"][0]["error"].as_str().unwrap();
                    assert!(why.starts_with("Thread post 1 of 1: "), "{why}");
                    assert!(why.contains("out of API credits"), "{why}");
                }

                #[tokio::test]
                #[serial]
                async fn a_post_that_will_not_come_down_says_why_and_stays_up() {
                    let (s, _, state, _, mock, cookie, _, set) = team().await;
                    let id = id_of(&create(&s, &cookie, x_post(&set, "stays")).await);
                    publish::tick(&state).await.unwrap();
                    mock.delete_mode.store(2, Ordering::SeqCst);
                    let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                    assert_eq!(res.status_code(), 422);
                    let msg = res.json::<Value>()["message"].as_str().unwrap().to_owned();
                    assert!(msg.contains("resets at 2026-11-04 12:30 UTC"), "{msg}");
                    assert!(msg.contains("Delete the post in the X app"), "{msg}");
                    assert_eq!(get_post(&s, &cookie, &id).await["status"], "published");

                    // Already deleted on X meanwhile: nothing left to take down.
                    mock.delete_mode.store(1, Ordering::SeqCst);
                    let res = req(&s, Method::DELETE, &format!("/v1/posts/{id}"), &cookie).await;
                    res.assert_status_ok();
                    assert_eq!(res.json::<Value>()["status"], "deleted");
                }

                #[tokio::test]
                #[serial]
                async fn an_expiring_token_is_renewed_first_and_the_new_refresh_token_saved() {
                    let (s, db, state, _, mock, cookie, team_id, set) = team().await;
                    let account = x_account(&db, &team_id).await;
                    let old = refresh_of(&account);
                    // Under two minutes left.
                    set_token(&db, account.id, "xtok", 60).await;
                    let id = id_of(&create(&s, &cookie, x_post(&set, "renewed")).await);
                    let tweets = mock.tweets.lock().unwrap().len();
                    publish::tick(&state).await.unwrap();
                    assert_eq!(get_post(&s, &cookie, &id).await["status"], "published");
                    assert_eq!(mock.refreshes.load(Ordering::SeqCst), 1);
                    let row = x_account(&db, &team_id).await;
                    assert!(access_of(&row).starts_with("xtok") && access_of(&row) != "xtok");
                    assert_ne!(refresh_of(&row), old);
                    assert!(row.meta.get("reconnect").is_none());
                    let left = row.token_expires_at.unwrap().timestamp() - Utc::now().timestamp();
                    assert!(left > 7000, "{left}");
                    // The tweet went out with the new access token.
                    assert_eq!(mock.tweets.lock().unwrap()[tweets].0, access_of(&row));

                    // Plenty left: no refresh.
                    let id = id_of(&create(&s, &cookie, x_post(&set, "again")).await);
                    publish::tick(&state).await.unwrap();
                    assert_eq!(get_post(&s, &cookie, &id).await["status"], "published");
                    assert_eq!(mock.refreshes.load(Ordering::SeqCst), 1);
                }

                #[tokio::test]
                #[serial]
                async fn a_refresh_without_a_new_refresh_token_keeps_the_current_one() {
                    let (_, db, state, _, mock, _, team_id, _) = team().await;
                    let account = x_account(&db, &team_id).await;
                    let old = refresh_of(&account);
                    mock.refresh_mode.store(1, Ordering::SeqCst);
                    set_token(&db, account.id, "xtok", -5).await;
                    let account = x_account(&db, &team_id).await;
                    let token = x::fresh_token(&state, &account).await.unwrap();
                    assert!(token.starts_with("xtok") && token != "xtok");
                    let row = x_account(&db, &team_id).await;
                    assert_eq!(refresh_of(&row), old);
                    assert_eq!(access_of(&row), token);
                }

                #[tokio::test]
                #[serial]
                async fn two_concurrent_refreshes_spend_the_refresh_token_once() {
                    let (_, db, state, _, mock, _, team_id, _) = team().await;
                    let account = x_account(&db, &team_id).await;
                    let old = refresh_of(&account);
                    set_token(&db, account.id, "xtok", -5).await;
                    let account = x_account(&db, &team_id).await;
                    let (a, b, c) = tokio::join!(
                        x::fresh_token(&state, &account),
                        x::fresh_token(&state, &account),
                        x::fresh_token(&state, &account),
                    );
                    let (a, b, c) = (a.unwrap(), b.unwrap(), c.unwrap());
                    assert!(a != "xtok" && a.starts_with("xtok"));
                    assert_eq!(a, b, "all three got the one new token");
                    assert_eq!(b, c);
                    assert_eq!(mock.refreshes.load(Ordering::SeqCst), 1, "X was asked once");
                    let row = x_account(&db, &team_id).await;
                    assert_ne!(refresh_of(&row), old);
                    assert!(row.meta.get("reconnect").is_none());
                    assert_eq!(access_of(&row), a);
                }

                #[tokio::test]
                #[serial]
                async fn a_refused_refresh_flags_the_account_until_it_is_connected_again() {
                    let (s, db, state, _, mock, cookie, team_id, set) = team().await;
                    let account = x_account(&db, &team_id).await;
                    // A refresh token X has never heard of (or already spent).
                    set_meta(&db, account.id, json!({"refresh_token": seal("spent")})).await;
                    set_token(&db, account.id, "xtok", -5).await;
                    let id = id_of(&create(&s, &cookie, x_post(&set, "a")).await);
                    let attempts = mock.attempts.load(Ordering::SeqCst);
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "failed");
                    assert_eq!(p["targets"][0]["error"], publish::X_RECONNECT);
                    assert_eq!(
                        mock.attempts.load(Ordering::SeqCst),
                        attempts,
                        "no tweet sent"
                    );
                    let row = x_account(&db, &team_id).await;
                    assert_eq!(row.meta["reconnect"], true);
                    assert_eq!(mock.refreshes.load(Ordering::SeqCst), 1);
                    // The connections list says so, for the reconnect prompt.
                    assert_eq!(list(&s, &cookie).await[0]["reconnect"], true);

                    // Flagged: X is not asked again, even by the job, and a live token is not used.
                    set_token(&db, account.id, "xtok", 3600).await;
                    let id = id_of(&create(&s, &cookie, x_post(&set, "b")).await);
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["targets"][0]["error"], publish::X_RECONNECT);
                    forget_others(&db, &team_id).await;
                    x::refresh_tokens(&state).await.unwrap();
                    assert_eq!(mock.refreshes.load(Ordering::SeqCst), 1);

                    // Signing in again clears it.
                    connect_with(&s, &cookie, "good").await;
                    let row = x_account(&db, &team_id).await;
                    assert!(row.meta.get("reconnect").is_none());
                    assert!(list(&s, &cookie).await[0].get("reconnect").is_none());
                    let id = id_of(&create(&s, &cookie, x_post(&set, "c")).await);
                    publish::tick(&state).await.unwrap();
                    assert_eq!(get_post(&s, &cookie, &id).await["status"], "published");
                }

                #[tokio::test]
                #[serial]
                async fn an_unreachable_refresh_does_not_flag_the_account() {
                    let (s, db, state, _, mock, cookie, team_id, set) = team().await;
                    let account = x_account(&db, &team_id).await;
                    let old = refresh_of(&account);
                    set_token(&db, account.id, "xtok", -5).await;
                    mock.refresh_mode.store(2, Ordering::SeqCst);
                    let id = id_of(&create(&s, &cookie, x_post(&set, "a")).await);
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "failed");
                    assert!(p["targets"][0]["error"]
                        .as_str()
                        .unwrap()
                        .contains("Refresh"));
                    let row = x_account(&db, &team_id).await;
                    assert!(row.meta.get("reconnect").is_none());
                    assert_eq!(refresh_of(&row), old, "the row is as it was");
                }

                #[tokio::test]
                #[serial]
                async fn a_token_revoked_on_x_asks_for_a_reconnect() {
                    let (s, db, state, _, mock, cookie, team_id, set) = team().await;
                    let account = x_account(&db, &team_id).await;
                    // Not expired, but X no longer knows it.
                    set_token(&db, account.id, "revoked", 3600).await;
                    let id = id_of(&create(&s, &cookie, x_post(&set, "a")).await);
                    let attempts = mock.attempts.load(Ordering::SeqCst);
                    publish::tick(&state).await.unwrap();
                    let p = get_post(&s, &cookie, &id).await;
                    assert_eq!(p["status"], "failed");
                    assert_eq!(p["targets"][0]["error"], publish::X_RECONNECT);
                    assert_eq!(mock.attempts.load(Ordering::SeqCst), attempts);
                }

                #[tokio::test]
                #[serial]
                async fn the_job_renews_tokens_near_their_end_and_skips_flagged_accounts() {
                    let (_, db, state, _, mock, _, team_id, _) = team().await;
                    forget_others(&db, &team_id).await;
                    let account = x_account(&db, &team_id).await;
                    // 40 minutes left: left alone.
                    set_token(&db, account.id, "xtok", 40 * 60).await;
                    assert_eq!(x::refresh_tokens(&state).await.unwrap(), 0);
                    assert_eq!(mock.refreshes.load(Ordering::SeqCst), 0);
                    // 20 minutes left: renewed.
                    set_token(&db, account.id, "xtok", 20 * 60).await;
                    assert_eq!(x::refresh_tokens(&state).await.unwrap(), 1);
                    let row = x_account(&db, &team_id).await;
                    assert!(access_of(&row).starts_with("xtok") && access_of(&row) != "xtok");
                    let left = row.token_expires_at.unwrap().timestamp() - Utc::now().timestamp();
                    assert!(left > 7000, "{left}");
                    assert_eq!(x::refresh_tokens(&state).await.unwrap(), 0, "fresh now");
                    // Flagged: skipped.
                    set_meta(&db, account.id, json!({"reconnect": true})).await;
                    set_token(&db, account.id, "xtok", -5).await;
                    assert_eq!(x::refresh_tokens(&state).await.unwrap(), 0);
                    assert_eq!(mock.refreshes.load(Ordering::SeqCst), 1);
                }
            }

            /// Meta: the connect flow with its Page picker, publishing to Facebook and
            /// Instagram through the Graph API mock, and take down.
            mod meta_api {
                use super::threads_api::{location, set_role};
                use super::*;
                use axum::{
                    extract::{Form, Path, Query},
                    routing::{get, post},
                    Json, Router,
                };
                use socialreins_brain::connectors::meta::Meta;
                use std::sync::atomic::{AtomicU64, Ordering};

                /// Every POST the mock took: (path, form), oldest first.
                static CALLS: std::sync::Mutex<Vec<(String, HashMap<String, String>)>> =
                    std::sync::Mutex::new(Vec::new());
                /// Every rupload `file_url`, and every deleted id.
                static UPLOADS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
                static REMOVED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
                static NEXT: AtomicU64 = AtomicU64::new(1);
                /// Every Reel thumbnail the mock took: (video id, content type, multipart body).
                /// `THUMB_MODE` 1: Meta refuses it.
                static THUMBS: std::sync::Mutex<Vec<(String, String, String)>> =
                    std::sync::Mutex::new(Vec::new());
                static THUMB_MODE: AtomicU64 = AtomicU64::new(0);

                type Answer = (axum::http::StatusCode, Json<Value>);

                fn ok(v: Value) -> Answer {
                    (axum::http::StatusCode::OK, Json(v))
                }

                fn refused(code: i64, message: &str) -> Answer {
                    let body = json!({"error": {"message": message, "code": code}});
                    (axum::http::StatusCode::BAD_REQUEST, Json(body))
                }

                fn refused_sub(code: i64, subcode: i64, message: &str) -> Answer {
                    let error = json!({"message": message, "code": code, "error_subcode": subcode});
                    (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(json!({ "error": error })),
                    )
                }

                fn bearer(h: &axum::http::HeaderMap) -> String {
                    let t = h.get("authorization").and_then(|v| v.to_str().ok());
                    t.unwrap_or_default().to_owned()
                }

                /// A Page token the mock handed out.
                fn page_token(h: &axum::http::HeaderMap) -> bool {
                    matches!(bearer(h).as_str(), "Bearer pt101" | "Bearer pt102")
                }

                fn next() -> u64 {
                    NEXT.fetch_add(1, Ordering::SeqCst)
                }

                /// The Graph API (v26.0), rupload, and a JPEG, a PNG and a video to point at.
                /// Code `good` grants Page 101 (with Instagram 1784) and, on the next cursor,
                /// Page 102; code `none` grants no Page.
                async fn mock_meta() -> String {
                    THUMB_MODE.store(0, Ordering::SeqCst);
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let base = format!("http://{}", listener.local_addr().unwrap());
                    let token = |Form(f): Form<HashMap<String, String>>| async move {
                        let get = |k: &str| f.get(k).map(String::as_str).unwrap_or_default();
                        if get("client_id") != "mapp" || get("client_secret") != "msecret" {
                            return refused(101, "bad app");
                        }
                        match (get("code"), get("grant_type"), get("fb_exchange_token")) {
                            ("good", "", _) => ok(json!({"access_token": "short"})),
                            ("none", "", _) => ok(json!({"access_token": "short-none"})),
                            ("", "fb_exchange_token", "short") => {
                                ok(json!({"access_token": "long"}))
                            }
                            ("", "fb_exchange_token", "short-none") => {
                                ok(json!({"access_token": "long-none"}))
                            }
                            _ => refused(100, "Invalid verification code"),
                        }
                    };
                    let accounts =
                        |h: axum::http::HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                            let fields = q.get("fields").map(String::as_str).unwrap_or_default();
                            assert!(fields.contains("instagram_business_account{"), "{fields}");
                            match (bearer(&h).as_str(), q.get("after").map(String::as_str)) {
                                ("Bearer long-none", _) => ok(json!({"data": []})),
                                ("Bearer long", None) => ok(json!({
                                    "data": [{"id": "101", "name": "Cafe", "access_token": "pt101",
                                        "picture": {"data": {"url": "https://scontent.example/101.jpg"}},
                                        "instagram_business_account": {"id": "1784",
                                            "username": "cafe",
                                            "profile_picture_url": "https://scontent.example/ig.jpg"}}],
                                    "paging": {"cursors": {"after": "A"},
                                        "next": "https://graph.facebook.com/next"}})),
                                ("Bearer long", Some("A")) => ok(json!({
                                    "data": [{"id": "102", "name": "Shop", "access_token": "pt102"}],
                                    "paging": {"cursors": {"after": "B"}}})),
                                _ => refused(190, "expired"),
                            }
                        };
                    let edge =
                        |h: axum::http::HeaderMap,
                         Path((node, edge)): Path<(String, String)>,
                         Form(f): Form<HashMap<String, String>>| async move {
                            if !page_token(&h) {
                                return refused(190, "Error validating access token");
                            }
                            let n = next();
                            let answer = match edge.as_str() {
                                "photos" if f.contains_key("published") => {
                                    json!({"id": n.to_string()})
                                }
                                "photos" => {
                                    json!({"id": n.to_string(), "post_id": format!("{node}_{n}")})
                                }
                                "feed" => json!({"id": format!("{node}_{n}")}),
                                "video_reels"
                                    if f.get("upload_phase").map(String::as_str)
                                        == Some("start") =>
                                {
                                    json!({"video_id": n.to_string()})
                                }
                                "video_reels" => json!({"success": true}),
                                "media" => json!({"id": n.to_string()}),
                                "media_publish" => json!({"id": n.to_string()}),
                                // Page 102 may not subscribe (permission missing).
                                "subscribed_apps" if node == "102" => {
                                    return refused(200, "(#200) pages_manage_metadata is missing")
                                }
                                "subscribed_apps" => json!({"success": true}),
                                // A first comment with REFUSE in it is turned down.
                                "comments"
                                    if f.get("message").is_some_and(|m| m.contains("REFUSE")) =>
                                {
                                    return refused(100, "(#100) Invalid comment")
                                }
                                "comments" | "replies" => json!({"id": format!("{node}_r{n}")}),
                                "messages" => {
                                    let to = f.get("recipient").map(String::as_str);
                                    let to = to.unwrap_or_default();
                                    // Comment 666 cannot be messaged, the text "LIMIT" is
                                    // rate limited, and "WINDOW" is outside the 24 hours.
                                    let said = f.get("message").map(String::as_str);
                                    let said = said.unwrap_or_default();
                                    if to.contains("666") {
                                        return refused_sub(100, 2_018_001, "No matching user");
                                    }
                                    if said.contains("LIMIT") {
                                        return refused(613, "Calls to this api have exceeded");
                                    }
                                    if said.contains("WINDOW") {
                                        return refused_sub(
                                            10,
                                            2_018_278,
                                            "Outside allowed window",
                                        );
                                    }
                                    json!({"recipient_id": "9", "message_id": format!("mid.{n}")})
                                }
                                _ => return refused(100, "unknown edge"),
                            };
                            CALLS.lock().unwrap().push((format!("{node}/{edge}"), f));
                            ok(answer)
                        };
                    let node =
                        |h: axum::http::HeaderMap,
                         Path(id): Path<String>,
                         Query(q): Query<HashMap<String, String>>| async move {
                            if !page_token(&h) {
                                return refused(190, "expired");
                            }
                            match q.get("fields").map(String::as_str) {
                                Some("status") => {
                                    ok(json!({"status": {"video_status": "processing",
                                    "uploading_phase": {"status": "complete"}}}))
                                }
                                Some("status_code,status") => {
                                    ok(json!({"status_code": "FINISHED"}))
                                }
                                Some("permalink") => ok(json!({
                                    "permalink": format!("https://www.instagram.com/p/X{id}/")})),
                                Some("name,profile_pic") => ok(json!({
                                    "name": "Dana Doe", "profile_pic": "https://scontent.example/d.jpg"})),
                                Some("name,username,profile_pic") => ok(json!({
                                    "name": "Ivy", "username": "ivy_ig",
                                    "profile_pic": "https://scontent.example/i.jpg"})),
                                _ => refused(100, "unknown fields"),
                            }
                        };
                    let unsubscribe =
                        |h: axum::http::HeaderMap, Path((node, edge)): Path<(String, String)>| async move {
                            if !page_token(&h) || edge != "subscribed_apps" {
                                return refused(190, "expired");
                            }
                            REMOVED.lock().unwrap().push(format!("{node}/{edge}"));
                            ok(json!({"success": true}))
                        };
                    let remove = |h: axum::http::HeaderMap, Path(id): Path<String>| async move {
                        if !page_token(&h) {
                            return refused(190, "expired");
                        }
                        REMOVED.lock().unwrap().push(id);
                        ok(json!({"success": true}))
                    };
                    let upload = |h: axum::http::HeaderMap| async move {
                        let auth = h.get("authorization").and_then(|v| v.to_str().ok());
                        if auth != Some("OAuth pt101") {
                            return refused(190, "expired");
                        }
                        let file = h.get("file_url").and_then(|v| v.to_str().ok());
                        UPLOADS
                            .lock()
                            .unwrap()
                            .push(file.unwrap_or_default().to_owned());
                        ok(json!({"success": true}))
                    };
                    let thumbnail = |h: axum::http::HeaderMap,
                                     Path(id): Path<String>,
                                     body: axum::body::Bytes| async move {
                        if !page_token(&h) || THUMB_MODE.load(Ordering::SeqCst) == 1 {
                            return refused(100, "(#100) Invalid thumbnail");
                        }
                        let kind = h.get("content-type").and_then(|v| v.to_str().ok());
                        THUMBS.lock().unwrap().push((
                            id,
                            kind.unwrap_or_default().to_owned(),
                            String::from_utf8_lossy(&body).into_owned(),
                        ));
                        ok(json!({"success": true}))
                    };
                    let mut jpeg = vec![7u8; 64];
                    jpeg[..4].copy_from_slice(b"\xFF\xD8\xFF\xE0");
                    let mut mp4 = vec![0u8; 64];
                    mp4[..12].copy_from_slice(b"\0\0\0\x18ftypisom");
                    let app = Router::new()
                        .route("/v26.0/oauth/access_token", post(token))
                        .route("/v26.0/{id}/thumbnails", post(thumbnail))
                        .route("/v26.0/me/accounts", get(accounts))
                        .route("/v26.0/{node}/{edge}", post(edge).delete(unsubscribe))
                        .route("/v26.0/{id}", get(node).delete(remove))
                        .route("/video-upload/v26.0/{id}", post(upload))
                        .route(
                            "/img.jpg",
                            get(move || async move { ([("content-type", "image/jpeg")], jpeg) }),
                        )
                        .route("/img.png", get(|| async { image("image/png", 64) }))
                        .route(
                            "/clip.mp4",
                            get(move || async move { ([("content-type", "video/mp4")], mp4) }),
                        );
                    tokio::spawn(async move { axum::serve(listener, app).await });
                    base
                }

                /// A brain with Meta set up on the mock (no waits), its state, and the mock.
                async fn on_meta(
                    configured: bool,
                ) -> (TestServer, DatabaseConnection, AppState, String) {
                    on_meta_with(configured, Some("vtoken")).await
                }

                /// `on_meta` with `META_WEBHOOK_VERIFY_TOKEN` set to `hook`.
                async fn on_meta_with(
                    configured: bool,
                    hook: Option<&str>,
                ) -> (TestServer, DatabaseConnection, AppState, String) {
                    let base = mock_meta().await;
                    let mut config = Config::from_env().expect("test config");
                    config.trust_proxy = true;
                    let app = configured.then(|| ("mapp".to_owned(), "msecret".to_owned()));
                    let mut m = Meta::new(app, "v26.0").unwrap();
                    m.graph_url = base.clone();
                    m.rupload_url = base.clone();
                    m.poll_every = std::time::Duration::ZERO;
                    m.webhook_token = hook.map(str::to_owned);
                    config.meta = m;
                    let disk = object_store::local::LocalFileSystem::new_with_prefix(media_dir())
                        .unwrap()
                        .with_automatic_cleanup(true);
                    config.media = Media::new(std::sync::Arc::new(disk), true);
                    let db = socialreins_brain::connect(&config)
                        .await
                        .expect("database reachable");
                    let state = AppState::new(db.clone(), &config);
                    let app = controllers::router(state.clone(), &config);
                    (TestServer::new(app).unwrap(), db, state, base)
                }

                /// Starts a connect: (state, `sr_meta=...`).
                async fn begin(s: &TestServer, cookie: &str) -> (String, String) {
                    let res = s
                        .get("/connections/meta/redirect")
                        .add_header("cookie", cookie)
                        .await;
                    let (to, q) = location(&res);
                    assert!(
                        to.starts_with("https://www.facebook.com/v26.0/dialog/oauth?"),
                        "{to}"
                    );
                    assert_eq!(q["client_id"], "mapp");
                    assert_eq!(
                        q["redirect_uri"],
                        "http://localhost:8080/connect/meta/callback"
                    );
                    assert!(q["scope"].contains("pages_manage_posts"));
                    assert!(q["scope"].contains("instagram_content_publish"));
                    let set = res
                        .iter_headers_by_name("set-cookie")
                        .map(|v| v.to_str().unwrap().to_owned())
                        .find(|c| c.starts_with("sr_meta="))
                        .unwrap();
                    assert!(set.contains("HttpOnly") && set.contains("Path=/connect/meta"));
                    (
                        q["state"].clone(),
                        set.split(';').next().unwrap().to_owned(),
                    )
                }

                /// A whole connect with `code`: the query web lands on.
                async fn connect_meta(
                    s: &TestServer,
                    cookie: &str,
                    code: &str,
                ) -> HashMap<String, String> {
                    let (state, flow) = begin(s, cookie).await;
                    let res = s
                        .get(&format!("/connect/meta/callback?code={code}&state={state}"))
                        .add_header("cookie", format!("{cookie}; {flow}"))
                        .await;
                    let (to, q) = location(&res);
                    assert!(
                        to.starts_with("http://localhost:3000/app/connections?"),
                        "{to}"
                    );
                    q
                }

                async fn choices(s: &TestServer, cookie: &str) -> axum_test::TestResponse {
                    s.get("/v1/connections/meta/pick")
                        .add_header("cookie", cookie)
                        .await
                }

                async fn pick(
                    s: &TestServer,
                    cookie: &str,
                    body: Value,
                ) -> axum_test::TestResponse {
                    req(s, Method::POST, "/v1/connections/meta/pick", cookie)
                        .json(&body)
                        .await
                }

                async fn account(
                    db: &DatabaseConnection,
                    team_id: &str,
                    provider: &str,
                    external_id: &str,
                ) -> social_accounts::Model {
                    social_accounts::Entity::find()
                        .filter(
                            social_accounts::Column::TeamId
                                .eq(team_id.parse::<uuid::Uuid>().unwrap()),
                        )
                        .filter(social_accounts::Column::Provider.eq(provider))
                        .filter(social_accounts::Column::ExternalId.eq(external_id))
                        .one(db)
                        .await
                        .unwrap()
                        .unwrap()
                }

                /// An owner with Page 101 and Instagram 1784 connected: (cookie, team id).
                async fn owner_connected(
                    s: &TestServer,
                    db: &DatabaseConnection,
                ) -> (String, String) {
                    let (_, cookie, _) = user(s, db).await;
                    let team_id = current(s, &cookie).await["id"].as_str().unwrap().to_owned();
                    assert_eq!(connect_meta(s, &cookie, "good").await["pick"], "meta");
                    let picked = json!({"facebook": ["101"], "instagram": ["1784"]});
                    assert_eq!(pick(s, &cookie, picked).await.status_code(), 201);
                    (cookie, team_id)
                }

                #[tokio::test]
                #[serial]
                async fn connect_offers_pages_and_instagram_accounts_to_pick() {
                    let (s, db, _, _) = on_meta(true).await;
                    let (_, cookie, user_id) = user(&s, &db).await;
                    let team_id = current(&s, &cookie).await["id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    assert_eq!(choices(&s, &cookie).await.status_code(), 404);
                    let q = connect_meta(&s, &cookie, "good").await;
                    assert_eq!(q.get("pick").map(String::as_str), Some("meta"));
                    assert!(!q.contains_key("connected") && !q.contains_key("connect_error"));

                    // Every Page across the paging cursor, and the linked Instagram account;
                    // never a token.
                    let res = choices(&s, &cookie).await;
                    res.assert_status_ok();
                    assert!(!res.text().contains("pt10"), "{}", res.text());
                    res.assert_json(&json!({
                        "facebook": [
                            {"id": "101", "name": "Cafe",
                             "picture_url": "https://scontent.example/101.jpg"},
                            {"id": "102", "name": "Shop", "picture_url": null}],
                        "instagram": [{"id": "1784", "name": "cafe",
                            "picture_url": "https://scontent.example/ig.jpg"}]}));

                    for (body, code) in [
                        (json!({}), "nothing_picked"),
                        (json!({"facebook": ["999"]}), "invalid_pick"),
                        (
                            json!({"facebook": ["101"], "instagram": ["101"]}),
                            "invalid_pick",
                        ),
                    ] {
                        let res = pick(&s, &cookie, body).await;
                        assert_eq!(res.status_code(), 422);
                        res.assert_json(&json!({"error": code}));
                    }
                    let picked = json!({"facebook": ["101"], "instagram": ["1784"]});
                    let res = pick(&s, &cookie, picked.clone()).await;
                    assert_eq!(res.status_code(), 201, "{}", res.text());
                    let saved: Vec<Value> = res.json();
                    assert_eq!(saved.len(), 2);
                    assert_eq!(saved[0]["provider"], "facebook");
                    assert_eq!(saved[0]["display_name"], "Cafe");
                    assert_eq!(saved[1]["provider"], "instagram");
                    assert_eq!(saved[1]["handle"], "cafe");
                    let set = default_set(&db, &team_id).await.to_string();
                    assert_eq!(saved[1]["set_ids"], json!([set]));
                    // The pick is used up.
                    assert_eq!(choices(&s, &cookie).await.status_code(), 404);
                    assert_eq!(pick(&s, &cookie, picked).await.status_code(), 404);

                    let fb = account(&db, &team_id, "facebook", "101").await;
                    assert_eq!(cipher().decrypt(&fb.access_token).unwrap(), b"pt101");
                    assert!(fb.token_expires_at.is_none());
                    let ig = account(&db, &team_id, "instagram", "1784").await;
                    assert_eq!(cipher().decrypt(&ig.access_token).unwrap(), b"pt101");
                    assert_eq!(ig.meta["page_id"], "101");
                    assert!(ig.token_expires_at.is_none());

                    // A reconnect refreshes what is connected at once and offers only the rest.
                    let row = social_accounts::ActiveModel {
                        id: Set(ig.id),
                        access_token: Set(cipher().encrypt(b"old")),
                        ..Default::default()
                    };
                    social_accounts::Entity::update(row)
                        .exec(&db)
                        .await
                        .unwrap();
                    assert_eq!(connect_meta(&s, &cookie, "good").await["pick"], "meta");
                    let ig = account(&db, &team_id, "instagram", "1784").await;
                    assert_eq!(cipher().decrypt(&ig.access_token).unwrap(), b"pt101");
                    choices(&s, &cookie).await.assert_json(&json!({
                        "facebook": [{"id": "102", "name": "Shop", "picture_url": null}],
                        "instagram": []}));
                    // Cancel drops it.
                    let res = req(&s, Method::DELETE, "/v1/connections/meta/pick", &cookie).await;
                    assert_eq!(res.status_code(), 204);
                    assert_eq!(choices(&s, &cookie).await.status_code(), 404);
                    // With everything connected there is nothing to pick.
                    connect_meta(&s, &cookie, "good").await;
                    let res = pick(&s, &cookie, json!({"facebook": ["102"]})).await;
                    assert_eq!(res.status_code(), 201);
                    let q = connect_meta(&s, &cookie, "good").await;
                    assert_eq!(q.get("connected").map(String::as_str), Some("meta"));
                    assert_eq!(list(&s, &cookie).await.len(), 3);

                    // Members cannot see or make a pick.
                    set_role(&db, user_id, "member").await;
                    assert_eq!(choices(&s, &cookie).await.status_code(), 403);
                    let res = pick(&s, &cookie, json!({"facebook": ["101"]})).await;
                    assert_eq!(res.status_code(), 403);
                }

                #[tokio::test]
                #[serial]
                async fn connect_errors_land_on_web_with_a_code() {
                    let (s, db, _, _) = on_meta(true).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let res = s.get("/connections/meta/redirect").await;
                    let q = location(&res).1;
                    assert_eq!(q["connect_error"], "unauthorized");
                    assert_eq!(q["provider"], "meta");
                    let err = |q: HashMap<String, String>| {
                        assert_eq!(q["provider"], "meta");
                        q["connect_error"].clone()
                    };
                    assert_eq!(err(connect_meta(&s, &cookie, "bad").await), "meta_failed");
                    assert_eq!(err(connect_meta(&s, &cookie, "none").await), "no_pages");
                    assert_eq!(err(connect_meta(&s, &cookie, "").await), "cancelled");
                    assert!(list(&s, &cookie).await.is_empty());

                    let (s, db, _, _) = on_meta(false).await;
                    let (_, cookie, _) = user(&s, &db).await;
                    let res = s
                        .get("/connections/meta/redirect")
                        .add_header("cookie", &cookie)
                        .await;
                    assert_eq!(location(&res).1["connect_error"], "not_configured");
                }

                fn meta_post(set: &str, platform: &str, body: &str) -> Value {
                    let mut v = input(set, body, Some(&past()));
                    v["platform"] = json!(platform);
                    v
                }

                /// Creates a due post with `media` on the mock, runs a tick, and returns the
                /// post and the POSTs it made.
                async fn publish_one(
                    (s, db, state, base): (&TestServer, &DatabaseConnection, &AppState, &str),
                    cookie: &str,
                    mut post: Value,
                    media: &[&str],
                ) -> (Value, Vec<(String, HashMap<String, String>)>) {
                    if !media.is_empty() {
                        // Valid placeholders; the mock's loopback URLs are set past validation.
                        post["media"] = json!(vec!["https://a.example/x.jpg"; media.len()]);
                    }
                    let res = create(s, cookie, post).await;
                    assert_eq!(res.status_code(), 201, "{}", res.text());
                    let id = id_of(&res);
                    set_media(db, base, &id, media).await;
                    let before = CALLS.lock().unwrap().len();
                    publish::tick(state).await.unwrap();
                    let calls = CALLS.lock().unwrap()[before..].to_vec();
                    (get_post(s, cookie, &id).await, calls)
                }

                #[tokio::test]
                #[serial]
                async fn facebook_posts_text_photos_and_reels_and_takes_them_down() {
                    let (s, db, state, base) = on_meta(true).await;
                    let (cookie, team_id) = owner_connected(&s, &db).await;
                    let set = default_set(&db, &team_id).await.to_string();
                    let ctx = (&s, &db, &state, base.as_str());

                    let (p, calls) =
                        publish_one(ctx, &cookie, meta_post(&set, "facebook", "hello"), &[]).await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(calls.len(), 1);
                    assert_eq!(calls[0].0, "101/feed");
                    assert_eq!(calls[0].1["message"], "hello");
                    let t = &p["targets"][0];
                    let id = t["remote_id"].as_str().unwrap().to_owned();
                    assert!(id.starts_with("101_"));
                    assert_eq!(t["remote_url"], format!("https://www.facebook.com/{id}"));
                    let res = req(
                        &s,
                        Method::DELETE,
                        &format!("/v1/posts/{}", p["id"].as_str().unwrap()),
                        &cookie,
                    )
                    .await;
                    res.assert_status_ok();
                    assert_eq!(res.json::<Value>()["status"], "deleted");
                    assert_eq!(REMOVED.lock().unwrap().last(), Some(&id));

                    let post = meta_post(&set, "facebook", "one");
                    let (p, calls) = publish_one(ctx, &cookie, post, &["/img.jpg"]).await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(calls.len(), 1);
                    assert_eq!(calls[0].0, "101/photos");
                    assert_eq!(calls[0].1["caption"], "one");
                    let link = &calls[0].1["url"];
                    assert!(link.starts_with("http://localhost:8080/media/"), "{link}");
                    let id = p["targets"][0]["remote_id"].as_str().unwrap();
                    assert!(id.starts_with("101_"), "the post id, not the photo id");

                    let post = meta_post(&set, "facebook", "two");
                    let (p, calls) =
                        publish_one(ctx, &cookie, post, &["/img.jpg", "/img.png"]).await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(calls.len(), 3);
                    for c in &calls[..2] {
                        assert_eq!(c.0, "101/photos");
                        assert_eq!(c.1["published"], "false");
                    }
                    assert_eq!(calls[2].0, "101/feed");
                    assert_eq!(calls[2].1["message"], "two");
                    let first: Value =
                        serde_json::from_str(&calls[2].1["attached_media[0]"]).unwrap();
                    assert!(first["media_fbid"].is_string());
                    assert!(calls[2].1.contains_key("attached_media[1]"));

                    let post = meta_post(&set, "facebook", "reel");
                    let (p, calls) = publish_one(ctx, &cookie, post, &["/clip.mp4"]).await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(calls.len(), 2);
                    assert_eq!(calls[0].0, "101/video_reels");
                    assert_eq!(calls[0].1["upload_phase"], "start");
                    assert_eq!(calls[1].1["upload_phase"], "finish");
                    assert_eq!(calls[1].1["video_state"], "PUBLISHED");
                    assert_eq!(calls[1].1["description"], "reel");
                    let video = calls[1].1["video_id"].clone();
                    assert_eq!(p["targets"][0]["remote_id"], video);
                    let uploaded = UPLOADS.lock().unwrap().last().cloned().unwrap();
                    assert!(uploaded.ends_with(".mp4"), "{uploaded}");

                    // A video cannot ride with other media; nothing goes out.
                    let post = meta_post(&set, "facebook", "mixed");
                    let (p, calls) =
                        publish_one(ctx, &cookie, post, &["/img.jpg", "/clip.mp4"]).await;
                    assert_eq!(p["status"], "failed");
                    assert!(calls.is_empty());
                    let error = p["targets"][0]["error"].as_str().unwrap();
                    assert!(error.contains("cannot mix a video"), "{error}");

                    // Thread parts are refused at save.
                    let mut post = meta_post(&set, "facebook", "x");
                    post["thread"] = json!([{"body": "two"}]);
                    let res = create(&s, &cookie, post).await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json(&json!({"error": "thread_not_supported"}));
                }

                #[tokio::test]
                #[serial]
                async fn a_first_comment_follows_the_post_and_a_take_down_leaves_it_to_the_post() {
                    let (s, db, state, base) = on_meta(true).await;
                    let (cookie, team_id) = owner_connected(&s, &db).await;
                    let set = default_set(&db, &team_id).await.to_string();
                    let ctx = (&s, &db, &state, base.as_str());

                    // Facebook: the Page comments on its post id with the Page token.
                    let mut post = meta_post(&set, "facebook", "hello");
                    post["first_comment"] = json!("Sources: example.org");
                    let (p, calls) = publish_one(ctx, &cookie, post, &[]).await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(p["first_comment"], "Sources: example.org");
                    let t = &p["targets"][0];
                    let id = t["remote_id"].as_str().unwrap().to_owned();
                    let paths: Vec<&str> = calls.iter().map(|c| c.0.as_str()).collect();
                    assert_eq!(paths, ["101/feed", &format!("{id}/comments")]);
                    assert_eq!(calls[1].1["message"], "Sources: example.org");
                    assert_eq!(t["status"], "published");
                    assert_eq!(t["comment_status"], "posted");
                    let comment = t["comment_id"].as_str().unwrap().to_owned();
                    assert!(comment.starts_with(&id), "{comment}");
                    assert_eq!(t["comment_error"], Value::Null);
                    let res = req(
                        &s,
                        Method::DELETE,
                        &format!("/v1/posts/{}", p["id"].as_str().unwrap()),
                        &cookie,
                    )
                    .await;
                    res.assert_status_ok();
                    assert_eq!(res.json::<Value>()["status"], "deleted");
                    // Only the post is deleted: the comment goes with it.
                    assert_eq!(REMOVED.lock().unwrap().last(), Some(&id));
                    assert!(!REMOVED.lock().unwrap().contains(&comment));

                    // Instagram: the account comments on the media id.
                    let mut post = meta_post(&set, "instagram", "pic");
                    post["first_comment"] = json!("Credit: Ann");
                    let (p, calls) = publish_one(ctx, &cookie, post, &["/img.jpg"]).await;
                    assert_eq!(p["status"], "published", "{p}");
                    let t = &p["targets"][0];
                    let id = t["remote_id"].as_str().unwrap();
                    assert_eq!(calls.len(), 3);
                    assert_eq!(calls[2].0, format!("{id}/comments"));
                    assert_eq!(calls[2].1["message"], "Credit: Ann");
                    assert_eq!(t["comment_status"], "posted");

                    // No first comment: nothing is sent and no status is set.
                    let (p, calls) =
                        publish_one(ctx, &cookie, meta_post(&set, "facebook", "plain"), &[]).await;
                    assert_eq!(calls.len(), 1);
                    assert_eq!(p["targets"][0]["comment_status"], Value::Null);
                }

                #[tokio::test]
                #[serial]
                async fn a_refused_first_comment_leaves_the_post_published_and_is_not_retried() {
                    let (s, db, state, base) = on_meta(true).await;
                    let (cookie, team_id) = owner_connected(&s, &db).await;
                    let set = default_set(&db, &team_id).await.to_string();
                    let mut post = meta_post(&set, "facebook", "hello");
                    post["first_comment"] = json!("REFUSE me");
                    let ctx = (&s, &db, &state, base.as_str());
                    let (p, calls) = publish_one(ctx, &cookie, post, &[]).await;
                    assert_eq!(calls.len(), 1, "the refused comment is not recorded");
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(p["error"], Value::Null);
                    let t = &p["targets"][0];
                    assert_eq!(t["status"], "published");
                    assert_eq!(t["error"], Value::Null);
                    assert_eq!(t["comment_status"], "failed");
                    assert_eq!(t["comment_id"], Value::Null);
                    let why = t["comment_error"].as_str().unwrap();
                    assert!(why.contains("Invalid comment"), "{why}");

                    // The target is sent again (queued, pending, unsent): the failed comment is
                    // not claimed again.
                    let id = p["id"].as_str().unwrap();
                    // Text the mock would take now, so a second claim would show as a call.
                    let fine = posts::ActiveModel {
                        id: Set(id.parse().unwrap()),
                        first_comment: Set(Some("fine now".into())),
                        ..Default::default()
                    };
                    posts::Entity::update(fine).exec(&db).await.unwrap();
                    requeue_unsent(&db, id).await;
                    let before = CALLS.lock().unwrap().len();
                    publish::publish_post(&state, id.parse().unwrap())
                        .await
                        .unwrap();
                    let calls = CALLS.lock().unwrap()[before..].to_vec();
                    let paths: Vec<&str> = calls.iter().map(|c| c.0.as_str()).collect();
                    assert_eq!(paths, ["101/feed"], "sent again, with no comment");
                    let again = get_post(&s, &cookie, id).await;
                    assert_eq!(again["targets"][0]["comment_status"], "failed");
                    assert_eq!(again["status"], "published");
                }

                /// Like `publish_one` for a video post with a cover from the mock's `cover` path.
                async fn publish_covered(
                    (s, db, state, base): (&TestServer, &DatabaseConnection, &AppState, &str),
                    cookie: &str,
                    mut post: Value,
                    (media, cover): (&[&str], &str),
                ) -> (Value, Vec<(String, HashMap<String, String>)>) {
                    post["media"] = json!(vec!["https://a.example/x.mp4"; media.len()]);
                    post["cover"] = json!("https://a.example/c.jpg");
                    let res = create(s, cookie, post).await;
                    assert_eq!(res.status_code(), 201, "{}", res.text());
                    let id = id_of(&res);
                    set_media(db, base, &id, media).await;
                    set_cover_url(db, base, &id, cover).await;
                    let before = CALLS.lock().unwrap().len();
                    publish::tick(state).await.unwrap();
                    let calls = CALLS.lock().unwrap()[before..].to_vec();
                    (get_post(s, cookie, &id).await, calls)
                }

                #[tokio::test]
                #[serial]
                async fn an_instagram_reel_takes_its_cover_by_url_in_the_container() {
                    let (s, db, state, base) = on_meta(true).await;
                    let (cookie, team_id) = owner_connected(&s, &db).await;
                    let set = default_set(&db, &team_id).await.to_string();
                    let ctx = (&s, &db, &state, base.as_str());

                    let post = meta_post(&set, "instagram", "reel");
                    let (p, calls) =
                        publish_covered(ctx, &cookie, post, (&["/clip.mp4"], "/img.jpg")).await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(p["targets"][0]["cover_error"], Value::Null);
                    assert_eq!(calls[0].1["media_type"], "REELS");
                    let cover = &calls[0].1["cover_url"];
                    assert!(cover.starts_with("http://localhost:8080/media/"), "{cover}");
                    assert!(cover.ends_with(".jpg"), "{cover}");
                    assert_eq!(calls[1].0, "1784/media_publish");

                    // Instagram takes a JPEG only; the post never starts.
                    let post = meta_post(&set, "instagram", "reel");
                    let (p, calls) =
                        publish_covered(ctx, &cookie, post, (&["/clip.mp4"], "/img.png")).await;
                    assert_eq!(p["status"], "failed", "{p}");
                    assert_eq!(
                        p["targets"][0]["error"],
                        "Instagram takes a JPEG cover image."
                    );
                    assert!(calls.is_empty());

                    // A cover on an image, or one that cannot be downloaded, is the same.
                    let post = meta_post(&set, "instagram", "pic");
                    let (p, calls) =
                        publish_covered(ctx, &cookie, post, (&["/img.jpg"], "/img.jpg")).await;
                    assert_eq!(p["status"], "failed", "{p}");
                    let error = p["targets"][0]["error"].as_str().unwrap();
                    assert_eq!(error, "A cover needs a post with exactly one video.");
                    let post = meta_post(&set, "instagram", "reel");
                    let (p, calls_missing) =
                        publish_covered(ctx, &cookie, post, (&["/clip.mp4"], "/nothing.jpg")).await;
                    assert_eq!(p["status"], "failed", "{p}");
                    let error = p["targets"][0]["error"].as_str().unwrap();
                    assert_eq!(error, "The cover image could not be downloaded (HTTP 404).");
                    assert!(calls.is_empty() && calls_missing.is_empty());
                }

                #[tokio::test]
                #[serial]
                async fn a_facebook_reel_gets_its_cover_as_a_thumbnail_once_it_is_up() {
                    let (s, db, state, base) = on_meta(true).await;
                    let (cookie, team_id) = owner_connected(&s, &db).await;
                    let set = default_set(&db, &team_id).await.to_string();
                    let ctx = (&s, &db, &state, base.as_str());

                    for (path, kind, ext) in [
                        ("/img.jpg", "image/jpeg", "jpg"),
                        ("/img.png", "image/png", "png"),
                    ] {
                        let before = THUMBS.lock().unwrap().len();
                        let post = meta_post(&set, "facebook", "reel");
                        let (p, calls) =
                            publish_covered(ctx, &cookie, post, (&["/clip.mp4"], path)).await;
                        assert_eq!(p["status"], "published", "{p}");
                        let t = &p["targets"][0];
                        assert_eq!(t["cover_error"], Value::Null);
                        // The Reel's own calls carry no cover; the thumbnail follows the finish.
                        assert_eq!(calls.len(), 2);
                        assert!(calls.iter().all(|c| !c.1.contains_key("thumb")));
                        let sent = THUMBS.lock().unwrap()[before..].to_vec();
                        assert_eq!(sent.len(), 1);
                        assert_eq!(sent[0].0, t["remote_id"].as_str().unwrap());
                        assert!(sent[0].1.starts_with("multipart/form-data; boundary="));
                        assert!(sent[0].2.contains("name=\"is_preferred\"\r\n\r\ntrue\r\n"));
                        assert!(sent[0].2.contains(&format!("filename=\"cover.{ext}\"")));
                        assert!(sent[0].2.contains(&format!("Content-Type: {kind}\r\n\r\n")));
                    }

                    // Meta refusing the thumbnail leaves the Reel published, with a warning.
                    THUMB_MODE.store(1, Ordering::SeqCst);
                    let post = meta_post(&set, "facebook", "reel");
                    let (p, _) =
                        publish_covered(ctx, &cookie, post, (&["/clip.mp4"], "/img.jpg")).await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(p["error"], Value::Null);
                    let t = &p["targets"][0];
                    assert_eq!(t["status"], "published");
                    let why = t["cover_error"].as_str().unwrap();
                    assert!(why.starts_with("The video is up, but its cover image was not set."));
                    assert!(why.contains("Cover:"), "{why}");
                    THUMB_MODE.store(0, Ordering::SeqCst);

                    // A cover on a photo post fails the target before anything goes out.
                    let post = meta_post(&set, "facebook", "pic");
                    let (p, calls) =
                        publish_covered(ctx, &cookie, post, (&["/img.jpg"], "/img.jpg")).await;
                    assert_eq!(p["status"], "failed", "{p}");
                    assert!(calls.is_empty());
                }

                #[tokio::test]
                #[serial]
                async fn instagram_posts_reels_and_carousels_and_takes_them_down() {
                    let (s, db, state, base) = on_meta(true).await;
                    let (cookie, team_id) = owner_connected(&s, &db).await;
                    let set = default_set(&db, &team_id).await.to_string();
                    let ctx = (&s, &db, &state, base.as_str());

                    let res = create(&s, &cookie, meta_post(&set, "instagram", "words")).await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json(&json!({"error": "media_required"}));

                    let post = meta_post(&set, "instagram", "pic");
                    let (p, calls) = publish_one(ctx, &cookie, post, &["/img.jpg"]).await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(calls.len(), 2);
                    assert_eq!(calls[0].0, "1784/media");
                    assert_eq!(calls[0].1["caption"], "pic");
                    assert!(calls[0].1["image_url"].starts_with("http://localhost:8080/media/"));
                    assert_eq!(calls[1].0, "1784/media_publish");
                    let t = &p["targets"][0];
                    let id = t["remote_id"].as_str().unwrap().to_owned();
                    assert_eq!(
                        t["remote_url"],
                        format!("https://www.instagram.com/p/X{id}/")
                    );
                    let res = req(
                        &s,
                        Method::DELETE,
                        &format!("/v1/posts/{}", p["id"].as_str().unwrap()),
                        &cookie,
                    )
                    .await;
                    res.assert_status_ok();
                    assert_eq!(REMOVED.lock().unwrap().last(), Some(&id));

                    let post = meta_post(&set, "instagram", "reel");
                    let (p, calls) = publish_one(ctx, &cookie, post, &["/clip.mp4"]).await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(calls[0].1["media_type"], "REELS");
                    assert!(calls[0].1["video_url"].ends_with(".mp4"));

                    let post = meta_post(&set, "instagram", "both");
                    let (p, calls) =
                        publish_one(ctx, &cookie, post, &["/img.jpg", "/clip.mp4"]).await;
                    assert_eq!(p["status"], "published", "{p}");
                    assert_eq!(calls.len(), 4);
                    assert_eq!(calls[0].1["media_type"], "IMAGE");
                    assert_eq!(calls[0].1["is_carousel_item"], "true");
                    assert_eq!(calls[1].1["media_type"], "VIDEO");
                    assert_eq!(calls[2].1["media_type"], "CAROUSEL");
                    assert_eq!(calls[2].1["caption"], "both");
                    assert_eq!(calls[2].1["children"].split(',').count(), 2);
                    assert!(!calls[2].1.contains_key("image_url"));
                    assert_eq!(calls[3].0, "1784/media_publish");

                    // Instagram takes JPEG images only; nothing goes out.
                    let post = meta_post(&set, "instagram", "png");
                    let (p, calls) = publish_one(ctx, &cookie, post, &["/img.png"]).await;
                    assert_eq!(p["status"], "failed");
                    assert!(calls.is_empty());
                    assert_eq!(
                        p["targets"][0]["error"],
                        "Image 1 is not a JPEG image or an MP4 or MOV video."
                    );

                    // A token Meta refuses fails the target with what to do.
                    let ig = account(&db, &team_id, "instagram", "1784").await;
                    let row = social_accounts::ActiveModel {
                        id: Set(ig.id),
                        access_token: Set(cipher().encrypt(b"dead")),
                        ..Default::default()
                    };
                    social_accounts::Entity::update(row)
                        .exec(&db)
                        .await
                        .unwrap();
                    let post = meta_post(&set, "instagram", "x");
                    let (p, _) = publish_one(ctx, &cookie, post, &["/img.jpg"]).await;
                    assert_eq!(p["status"], "failed");
                    assert_eq!(p["targets"][0]["error"], publish::META_RECONNECT);
                }

                // The Meta webhook: handshake, signatures, ingest, replies, subscriptions.

                use sea_orm::QueryOrder;
                use socialreins_brain::{
                    inbox::{self, Via},
                    models::{inbox_items, inbox_replies},
                };

                /// Meta accounts left by earlier tests would take part in these deliveries.
                async fn clean(db: &DatabaseConnection) {
                    social_accounts::Entity::delete_many()
                        .filter(social_accounts::Column::Provider.is_in(["facebook", "instagram"]))
                        .exec(db)
                        .await
                        .unwrap();
                    CALLS.lock().unwrap().clear();
                    REMOVED.lock().unwrap().clear();
                }

                /// A delivery as Meta signs it with the app secret.
                async fn deliver(s: &TestServer, payload: &Value) -> axum_test::TestResponse {
                    let body = serde_json::to_vec(payload).unwrap();
                    let sig = socialreins_core::webhook::sign_sha256("msecret", &body);
                    s.post("/webhooks/meta")
                        .add_header("x-hub-signature-256", sig)
                        .content_type("application/json")
                        .bytes(body.into())
                        .await
                }

                fn now() -> i64 {
                    chrono::Utc::now().timestamp()
                }

                fn page_comment(comment: &str, text: &str, from: &str, when: i64) -> Value {
                    json!({"object": "page", "entry": [{"id": "101", "time": when, "changes": [
                        {"field": "feed", "value": {"item": "comment", "verb": "add",
                            "comment_id": comment, "post_id": "101_9", "parent_id": "101_9",
                            "from": {"id": from, "name": "Ada Fan"}, "message": text,
                            "created_time": when}}]}]})
                }

                fn page_message(entry: &str, mid: &str, from: &str, text: &str) -> Value {
                    let mut message = json!({"mid": mid, "text": text});
                    if from == entry {
                        message["is_echo"] = json!(true);
                    }
                    json!({"object": "page", "entry": [{"id": entry, "time": now(), "messaging": [
                        {"sender": {"id": from}, "recipient": {"id": "101"},
                         "timestamp": now() * 1000, "message": message}]}]})
                }

                fn ig_comment(entry: &str, id: &str, from: &str) -> Value {
                    ig_comment_as(entry, id, from, "jane")
                }

                fn ig_comment_as(entry: &str, id: &str, from: &str, username: &str) -> Value {
                    json!({"object": "instagram", "entry": [{"id": entry, "time": now(), "changes": [
                        {"field": "comments", "value": {"id": id, "text": "so nice",
                            "from": {"id": from, "username": username},
                            "media": {"id": "17900000", "media_product_type": "FEED"}}}]}]})
                }

                fn ig_message(entry: &str, mid: &str, from: &str) -> Value {
                    let mut message = json!({"mid": mid, "text": "hi from ig"});
                    if from == entry {
                        message["is_echo"] = json!(true);
                    }
                    json!({"object": "instagram", "entry": [{"id": entry, "time": now(), "messaging": [
                        {"sender": {"id": from}, "recipient": {"id": "1784"},
                         "timestamp": now() * 1000, "message": message}]}]})
                }

                async fn stored(db: &DatabaseConnection, team_id: &str) -> Vec<inbox_items::Model> {
                    inbox_items::Entity::find()
                        .filter(
                            inbox_items::Column::TeamId.eq(team_id.parse::<uuid::Uuid>().unwrap()),
                        )
                        .order_by_asc(inbox_items::Column::RemoteId)
                        .all(db)
                        .await
                        .unwrap()
                }

                /// Polls `f` until it gives something: the work after the `200` takes a moment.
                async fn until<T, F: std::future::Future<Output = Option<T>>>(
                    mut f: impl FnMut() -> F,
                ) -> T {
                    for _ in 0..100 {
                        if let Some(found) = f().await {
                            return found;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    panic!("nothing happened in 5 seconds");
                }

                fn calls_since(before: usize) -> Vec<(String, HashMap<String, String>)> {
                    CALLS.lock().unwrap()[before..].to_vec()
                }

                async fn inbox_items_of(s: &TestServer, cookie: &str, inbox: &str) -> Vec<Value> {
                    let res = s
                        .get(&format!("/v1/inboxes/{inbox}/items"))
                        .add_header("cookie", cookie)
                        .await;
                    res.assert_status_ok();
                    res.json::<Value>()["items"].as_array().unwrap().clone()
                }

                async fn all_inbox_id(s: &TestServer, cookie: &str) -> String {
                    let res = s.get("/v1/inboxes").add_header("cookie", cookie).await;
                    res.assert_status_ok();
                    res.json::<Value>()[0]["id"].as_str().unwrap().to_owned()
                }

                #[tokio::test]
                #[serial]
                async fn the_handshake_echoes_the_challenge_for_the_right_token_only() {
                    let (s, _, _, _) = on_meta(true).await;
                    let base = "/webhooks/meta?hub.mode=subscribe&hub.challenge=1158201444";
                    let res = s.get(&format!("{base}&hub.verify_token=vtoken")).await;
                    res.assert_status_ok();
                    assert_eq!(res.text(), "1158201444");
                    let kind = res.header("content-type");
                    assert!(kind.to_str().unwrap().starts_with("text/plain"));
                    for query in [
                        format!("{base}&hub.verify_token=vtokeo"),
                        format!("{base}&hub.verify_token=vtoken2"),
                        format!("{base}&hub.verify_token="),
                        base.to_owned(),
                        "/webhooks/meta?hub.mode=unsubscribe&hub.verify_token=vtoken&hub.challenge=1"
                            .to_owned(),
                        "/webhooks/meta?hub.mode=subscribe&hub.verify_token=vtoken".to_owned(),
                        "/webhooks/meta".to_owned(),
                    ] {
                        let res = s.get(&query).await;
                        assert_eq!(res.status_code(), 403, "{query}");
                        res.assert_json(&json!({"error": "forbidden"}));
                    }
                    // Without META_WEBHOOK_VERIFY_TOKEN the webhook is off.
                    let (s, _, _, _) = on_meta_with(true, None).await;
                    let res = s.get(&format!("{base}&hub.verify_token=")).await;
                    assert_eq!(res.status_code(), 404);
                }

                #[tokio::test]
                #[serial]
                async fn deliveries_need_a_valid_signature_before_anything_is_read() {
                    let (s, db, _, _) = on_meta(true).await;
                    clean(&db).await;
                    let (_, team_id) = owner_connected(&s, &db).await;
                    let payload = page_comment("101_9_1", "hi", "777", now());
                    let body = serde_json::to_vec(&payload).unwrap();
                    let good = socialreins_core::webhook::sign_sha256("msecret", &body);
                    let tampered = {
                        let mut other = payload.clone();
                        other["entry"][0]["changes"][0]["value"]["message"] = json!("bye");
                        serde_json::to_vec(&other).unwrap()
                    };
                    let post = |body: Vec<u8>, sig: Option<&str>| {
                        let mut r = s.post("/webhooks/meta").content_type("application/json");
                        if let Some(sig) = sig {
                            r = r.add_header("x-hub-signature-256", sig.to_owned());
                        }
                        r.bytes(body.into())
                    };
                    let zeros = format!("sha256={}", "0".repeat(64));
                    let wrong_key = socialreins_core::webhook::sign_sha256("other", &body);
                    let sha1 = good.replace("sha256=", "sha1=");
                    let uppercase = good.to_uppercase();
                    let cases = [
                        (body.clone(), None),
                        (body.clone(), Some(zeros.as_str())),
                        (body.clone(), Some("")),
                        (body.clone(), Some(wrong_key.as_str())),
                        (body.clone(), Some(sha1.as_str())),
                        (body.clone(), Some(uppercase.as_str())),
                        (body.clone(), Some(&good[7..])),
                        (tampered, Some(good.as_str())),
                    ];
                    for (i, (body, sig)) in cases.into_iter().enumerate() {
                        let res = post(body, sig).await;
                        assert_eq!(res.status_code(), 403, "case {i}");
                        res.assert_json(&json!({"error": "forbidden"}));
                    }
                    assert!(
                        stored(&db, &team_id).await.is_empty(),
                        "no work for a bad one"
                    );

                    assert_eq!(post(body.clone(), Some(&good)).await.status_code(), 200);
                    assert_eq!(stored(&db, &team_id).await.len(), 1);
                    // A signed body that is not JSON, or says nothing we use, is still a 200.
                    let junk = b"not json".to_vec();
                    let sig = socialreins_core::webhook::sign_sha256("msecret", &junk);
                    assert_eq!(post(junk, Some(&sig)).await.status_code(), 200);
                    let other =
                        json!({"object": "whatsapp_business_account", "entry": [{"id": "1"}]});
                    assert_eq!(deliver(&s, &other).await.status_code(), 200);
                    // A body over the cap is refused outright.
                    let big = vec![b' '; 300 * 1024];
                    let sig = socialreins_core::webhook::sign_sha256("msecret", &big);
                    assert_eq!(post(big, Some(&sig)).await.status_code(), 413);
                    assert_eq!(stored(&db, &team_id).await.len(), 1);

                    // No app secret, no webhook.
                    let (s, _, _, _) = on_meta(false).await;
                    let res = s
                        .post("/webhooks/meta")
                        .add_header("x-hub-signature-256", good.clone())
                        .bytes(body.into())
                        .await;
                    assert_eq!(res.status_code(), 404);
                }

                #[tokio::test]
                #[serial]
                async fn facebook_comments_and_messages_land_in_the_inbox() {
                    let (s, db, _, _) = on_meta(true).await;
                    clean(&db).await;
                    let (cookie, team_id) = owner_connected(&s, &db).await;
                    let when = now();
                    let res = deliver(&s, &page_comment("101_9_5", "Love it", "777", when)).await;
                    assert_eq!(res.status_code(), 200);
                    let res = deliver(&s, &page_message("101", "m_1", "777", "hello")).await;
                    assert_eq!(res.status_code(), 200);
                    let items = stored(&db, &team_id).await;
                    assert_eq!(items.len(), 2);
                    let fb = account(&db, &team_id, "facebook", "101").await;
                    let comment = items.iter().find(|i| i.kind == "comment").unwrap();
                    assert_eq!(comment.social_account_id, fb.id);
                    assert_eq!(comment.platform, "facebook");
                    assert_eq!(comment.remote_id, "101_9_5");
                    assert_eq!(comment.parent_remote_id.as_deref(), Some("101_9"));
                    assert_eq!(comment.root_remote_id.as_deref(), Some("101_9"));
                    assert_eq!(comment.author_id, "777");
                    assert_eq!(comment.author_name.as_deref(), Some("Ada Fan"));
                    assert_eq!(comment.text, "Love it");
                    assert_eq!(comment.received_at.timestamp(), when);
                    assert_eq!(comment.state, "open");
                    let dm = items.iter().find(|i| i.kind == "dm").unwrap();
                    assert_eq!(dm.remote_id, "m_1");
                    assert_eq!(dm.convo_id.as_deref(), Some("777"));
                    assert_eq!(dm.text, "hello");

                    // The sender's name comes from a lookup after the 200.
                    let id = dm.id;
                    let named = until(|| async {
                        let row = inbox_items::Entity::find_by_id(id).one(&db).await.unwrap();
                        row.filter(|r| r.author_name.is_some())
                    })
                    .await;
                    assert_eq!(named.author_name.as_deref(), Some("Dana Doe"));
                    assert_eq!(
                        named.author_avatar.as_deref(),
                        Some("https://scontent.example/d.jpg")
                    );

                    // The team's inbox shows them, with the post they hang off and a link.
                    let all = all_inbox_id(&s, &cookie).await;
                    let page = inbox_items_of(&s, &cookie, &all).await;
                    assert_eq!(page.len(), 2);
                    let c = page.iter().find(|i| i["kind"] == "comment").unwrap();
                    assert_eq!(c["platform"], "facebook");
                    assert_eq!(c["parent_remote_id"], "101_9");
                    assert_eq!(c["remote_url"], "https://www.facebook.com/101_9");
                    assert_eq!(c["author"]["name"], "Ada Fan");
                }

                #[tokio::test]
                #[serial]
                async fn instagram_comments_and_messages_land_in_the_inbox() {
                    let (s, db, _, _) = on_meta(true).await;
                    clean(&db).await;
                    let (cookie, team_id) = owner_connected(&s, &db).await;
                    assert_eq!(
                        deliver(&s, &ig_comment("1784", "1790", "555"))
                            .await
                            .status_code(),
                        200
                    );
                    assert_eq!(
                        deliver(&s, &ig_message("1784", "ig_m1", "555"))
                            .await
                            .status_code(),
                        200
                    );
                    let items = stored(&db, &team_id).await;
                    assert_eq!(items.len(), 2);
                    let comment = items.iter().find(|i| i.kind == "comment").unwrap();
                    assert_eq!(comment.platform, "instagram");
                    assert_eq!(comment.author_handle.as_deref(), Some("jane"));
                    assert_eq!(comment.parent_remote_id.as_deref(), Some("17900000"));
                    assert_eq!(comment.root_remote_id.as_deref(), Some("17900000"));
                    let ig = account(&db, &team_id, "instagram", "1784").await;
                    assert_eq!(comment.social_account_id, ig.id);
                    // Its link and the sender's profile are looked up after the 200.
                    let (cid, mid) = (
                        comment.id,
                        items.iter().find(|i| i.kind == "dm").unwrap().id,
                    );
                    let linked = until(|| async {
                        let row = inbox_items::Entity::find_by_id(cid).one(&db).await.unwrap();
                        row.filter(|r| r.permalink.is_some())
                    })
                    .await;
                    assert_eq!(
                        linked.permalink.as_deref(),
                        Some("https://www.instagram.com/p/X17900000/")
                    );
                    let named = until(|| async {
                        let row = inbox_items::Entity::find_by_id(mid).one(&db).await.unwrap();
                        row.filter(|r| r.author_name.is_some())
                    })
                    .await;
                    assert_eq!(named.author_handle.as_deref(), Some("ivy_ig"));
                    assert_eq!(named.author_name.as_deref(), Some("Ivy"));
                    assert_eq!(named.convo_id.as_deref(), Some("555"));

                    // An inbox for Instagram only shows these; one for Facebook, nothing.
                    for (platform, want) in [("instagram", 2), ("facebook", 0)] {
                        let res = req(&s, Method::POST, "/v1/inboxes", &cookie)
                            .json(&json!({"name": platform, "filters": {"platforms": [platform]}}))
                            .await;
                        assert_eq!(res.status_code(), 201, "{}", res.text());
                        let id = res.json::<Value>()["id"].as_str().unwrap().to_owned();
                        assert_eq!(
                            inbox_items_of(&s, &cookie, &id).await.len(),
                            want,
                            "{platform}"
                        );
                    }
                }

                #[tokio::test]
                #[serial]
                async fn echoes_own_comments_unknown_accounts_and_repeats_are_ignored() {
                    let (s, db, _, _) = on_meta(true).await;
                    clean(&db).await;
                    let (_, team_id) = owner_connected(&s, &db).await;
                    for payload in [
                        // Our own comment, and a reply from our own Page.
                        page_comment("101_9_1", "thanks!", "101", now()),
                        // What the Page itself sent (an echo).
                        page_message("101", "m_echo", "101", "sent by us"),
                        ig_comment("1784", "1791", "1784"),
                        ig_message("1784", "ig_echo", "1784"),
                        // A Page and an Instagram account nobody connected.
                        page_message("999", "m_2", "777", "who am i"),
                        ig_comment("555000", "1792", "555"),
                        json!({"object": "page", "entry": [{"id": "101", "changes": [
                            {"field": "feed", "value": {"item": "reaction", "verb": "add"}},
                            {"field": "feed", "value": {"item": "comment", "verb": "remove",
                                "comment_id": "101_9_2", "post_id": "101_9",
                                "from": {"id": "777"}}},
                            {"field": "feed", "value": {"item": "status", "verb": "add"}}]}]}),
                    ] {
                        assert_eq!(deliver(&s, &payload).await.status_code(), 200);
                    }
                    assert!(stored(&db, &team_id).await.is_empty());

                    // The same delivery twice, even in one body, adds one item.
                    let once = page_comment("101_9_3", "again", "777", now());
                    let twice = json!({"object": "page", "entry": [
                        once["entry"][0].clone(), once["entry"][0].clone()]});
                    for payload in [&once, &once, &twice] {
                        assert_eq!(deliver(&s, payload).await.status_code(), 200);
                    }
                    let dm = page_message("101", "m_3", "777", "dup");
                    assert_eq!(deliver(&s, &dm).await.status_code(), 200);
                    assert_eq!(deliver(&s, &dm).await.status_code(), 200);
                    let items = stored(&db, &team_id).await;
                    assert_eq!(
                        items.len(),
                        2,
                        "{:?}",
                        items.iter().map(|i| &i.remote_id).collect::<Vec<_>>()
                    );
                }

                #[tokio::test]
                #[serial]
                async fn each_team_with_the_account_gets_its_own_items() {
                    let (s, db, _, _) = on_meta(true).await;
                    clean(&db).await;
                    let (cookie_a, team_a) = owner_connected(&s, &db).await;
                    let (cookie_b, team_b) = owner_connected(&s, &db).await;
                    let (_, cookie_c, _) = user(&s, &db).await;
                    let res = deliver(&s, &page_comment("101_9_7", "to both", "777", now())).await;
                    assert_eq!(res.status_code(), 200);
                    let (a, b) = (stored(&db, &team_a).await, stored(&db, &team_b).await);
                    assert_eq!((a.len(), b.len()), (1, 1));
                    assert_ne!(a[0].id, b[0].id);
                    assert_ne!(a[0].social_account_id, b[0].social_account_id);
                    assert_eq!(a[0].team_id.to_string(), team_a);
                    assert_eq!(b[0].team_id.to_string(), team_b);
                    for (cookie, want) in [(&cookie_a, a[0].id), (&cookie_b, b[0].id)] {
                        let all = all_inbox_id(&s, cookie).await;
                        let page = inbox_items_of(&s, cookie, &all).await;
                        assert_eq!(page.len(), 1);
                        assert_eq!(page[0]["id"], want.to_string());
                    }
                    // A team without the account sees nothing, and cannot open another's item.
                    let all = all_inbox_id(&s, &cookie_c).await;
                    assert!(inbox_items_of(&s, &cookie_c, &all).await.is_empty());
                    let res = s
                        .get(&format!("/v1/inbox-items/{}", a[0].id))
                        .add_header("cookie", &cookie_c)
                        .await;
                    assert_eq!(res.status_code(), 404);
                }

                #[tokio::test]
                #[serial]
                async fn connecting_subscribes_the_page_and_disconnecting_unsubscribes() {
                    let (s, db, _, _) = on_meta(true).await;
                    clean(&db).await;
                    let (cookie, team_id) = owner_connected(&s, &db).await;
                    // Page 101 once, though its Facebook and Instagram accounts were both saved.
                    let subscribed = calls_since(0)
                        .into_iter()
                        .filter(|(path, _)| path == "101/subscribed_apps")
                        .collect::<Vec<_>>();
                    assert_eq!(subscribed.len(), 1);
                    assert_eq!(subscribed[0].1["subscribed_fields"], "feed,messages");
                    let accounts = list(&s, &cookie).await;
                    assert!(accounts.iter().all(|a| a.get("webhook_error").is_none()));

                    // A refusal never fails the connect, but shows on the account.
                    connect_meta(&s, &cookie, "good").await;
                    let res = pick(&s, &cookie, json!({"facebook": ["102"]})).await;
                    assert_eq!(res.status_code(), 201, "{}", res.text());
                    let saved: Vec<Value> = res.json();
                    let why = saved[0]["webhook_error"].as_str().unwrap();
                    assert!(why.contains("pages_manage_metadata"), "{why}");
                    assert!(!why.contains("pt102"));
                    let shop = account(&db, &team_id, "facebook", "102").await;
                    assert_eq!(shop.meta["webhook_error"], why);
                    // A reconnect subscribes every connected Page again (101 once, 102 refuses).
                    let before = calls_since(0).len();
                    connect_meta(&s, &cookie, "good").await;
                    assert_eq!(calls_since(before).len(), 1);
                    assert!(list(&s, &cookie)
                        .await
                        .iter()
                        .any(|a| a["webhook_error"] == why));

                    // The Page stays subscribed while one of its accounts is connected.
                    let fb = account(&db, &team_id, "facebook", "101").await;
                    let ig = account(&db, &team_id, "instagram", "1784").await;
                    let res = req(
                        &s,
                        Method::DELETE,
                        &format!("/v1/connections/{}", fb.id),
                        &cookie,
                    )
                    .await;
                    assert_eq!(res.status_code(), 204);
                    assert!(REMOVED.lock().unwrap().is_empty());
                    let res = req(
                        &s,
                        Method::DELETE,
                        &format!("/v1/connections/{}", ig.id),
                        &cookie,
                    )
                    .await;
                    assert_eq!(res.status_code(), 204);
                    assert_eq!(*REMOVED.lock().unwrap(), ["101/subscribed_apps"]);

                    // Nor does it come off while another team has the Page.
                    let (other, _) = owner_connected(&s, &db).await;
                    let (mine, _) = owner_connected(&s, &db).await;
                    REMOVED.lock().unwrap().clear();
                    for a in list(&s, &mine).await {
                        let id = a["id"].as_str().unwrap();
                        let path = format!("/v1/connections/{id}");
                        let res = req(&s, Method::DELETE, &path, &mine).await;
                        assert_eq!(res.status_code(), 204);
                    }
                    assert!(REMOVED.lock().unwrap().is_empty());
                    assert_eq!(list(&s, &other).await.len(), 2);
                }

                #[tokio::test]
                #[serial]
                async fn replies_and_dms_go_out_through_the_graph_api_once() {
                    let (s, db, _, _) = on_meta(true).await;
                    clean(&db).await;
                    let (cookie, team_id) = owner_connected(&s, &db).await;
                    deliver(&s, &page_comment("101_9_5", "Love it", "777", now())).await;
                    deliver(&s, &ig_comment("1784", "1790", "555")).await;
                    deliver(&s, &page_message("101", "m_1", "777", "hello")).await;
                    deliver(&s, &ig_message("1784", "ig_m1", "555")).await;
                    let items = stored(&db, &team_id).await;
                    let item =
                        |remote: &str| items.iter().find(|i| i.remote_id == remote).unwrap().id;
                    let reply = |id: uuid::Uuid, text: &str| {
                        req(
                            &s,
                            Method::POST,
                            &format!("/v1/inbox-items/{id}/reply"),
                            &cookie,
                        )
                        .json(&json!({ "text": text }))
                    };

                    // A public reply: /comments on Facebook, /replies on Instagram.
                    let before = calls_since(0).len();
                    let res = reply(item("101_9_5"), "Thank you!").await;
                    assert_eq!(res.status_code(), 201, "{}", res.text());
                    let body = res.json::<Value>();
                    assert_eq!(body["replies"][0]["kind"], "public");
                    assert!(body["replied_at"].is_string());
                    let res = reply(item("1790"), "Glad you like it").await;
                    assert_eq!(res.status_code(), 201, "{}", res.text());
                    let calls = calls_since(before);
                    assert_eq!(calls[0].0, "101_9_5/comments");
                    assert_eq!(calls[0].1["message"], "Thank you!");
                    assert_eq!(calls[1].0, "1790/replies");
                    assert_eq!(calls[1].1["message"], "Glad you like it");

                    // A DM goes from the Page (Instagram through its linked Page) to the person.
                    let before = calls_since(0).len();
                    let res = reply(item("m_1"), "Hi Dana").await;
                    assert_eq!(res.status_code(), 201, "{}", res.text());
                    assert_eq!(res.json::<Value>()["replies"][0]["kind"], "dm");
                    assert_eq!(reply(item("ig_m1"), "Hi Ivy").await.status_code(), 201);
                    let calls = calls_since(before);
                    assert_eq!(calls[0].0, "101/messages");
                    assert_eq!(calls[0].1["recipient"], r#"{"id":"777"}"#);
                    assert_eq!(calls[0].1["message"], r#"{"text":"Hi Dana"}"#);
                    assert_eq!(calls[0].1["messaging_type"], "RESPONSE");
                    assert_eq!(calls[1].0, "101/messages");
                    assert_eq!(calls[1].1["recipient"], r#"{"id":"555"}"#);

                    // What Meta refuses comes back as a reason, never retried.
                    let before = calls_since(0).len();
                    let res = reply(item("m_1"), "LIMIT").await;
                    assert_eq!(res.status_code(), 429);
                    let res = reply(item("m_1"), "WINDOW").await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json_contains(&json!({"error": "outside_window"}));
                    assert_eq!(
                        calls_since(before).len(),
                        0,
                        "the mock refused before recording"
                    );
                    let res = reply(item("m_1"), &"a".repeat(2001)).await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json(&json!({"error": "invalid_text"}));
                    let res = reply(item("1790"), &"a".repeat(2201)).await;
                    assert_eq!(res.status_code(), 422);

                    // A DM more than 24 hours after the person's last message is not sent.
                    let old = (chrono::Utc::now() - chrono::Duration::hours(25)).fixed_offset();
                    for remote in ["m_1", "ig_m1"] {
                        let row = inbox_items::ActiveModel {
                            id: Set(item(remote)),
                            received_at: Set(old),
                            ..Default::default()
                        };
                        inbox_items::Entity::update(row).exec(&db).await.unwrap();
                    }
                    let before = calls_since(0).len();
                    let res = reply(item("m_1"), "Late hello").await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json_contains(&json!({"error": "outside_window"}));
                    assert_eq!(calls_since(before).len(), 0);

                    // A token Meta refuses asks for a new connect, in Meta's words.
                    let fb = account(&db, &team_id, "facebook", "101").await;
                    let row = social_accounts::ActiveModel {
                        id: Set(fb.id),
                        access_token: Set(cipher().encrypt(b"dead")),
                        ..Default::default()
                    };
                    social_accounts::Entity::update(row)
                        .exec(&db)
                        .await
                        .unwrap();
                    let res = reply(item("101_9_5"), "Second thanks").await;
                    assert_eq!(res.status_code(), 422);
                    res.assert_json_contains(&json!({"error": "meta_reconnect_required"}));
                    let sent = inbox_replies::Entity::find()
                        .filter(inbox_replies::Column::InboxItemId.eq(item("101_9_5")))
                        .all(&db)
                        .await
                        .unwrap();
                    assert_eq!(sent.len(), 1, "only the first reply went out");
                }

                #[tokio::test]
                #[serial]
                async fn comment_automations_reply_privately_once_with_a_public_fallback() {
                    let (s, db, state, _) = on_meta(true).await;
                    clean(&db).await;
                    let (cookie, team_id) = owner_connected(&s, &db).await;
                    let res = req(&s, Method::POST, "/v1/automations", &cookie)
                        .json(&json!({
                            "name": "Guide by DM", "trigger": "comment_received",
                            "conditions": {},
                            "actions": [{"type": "send_dm", "text": "Here is the guide: https://x.io"}],
                            "fallback_public_reply": "Please check your DMs",
                        }))
                        .await;
                    assert_eq!(res.status_code(), 201, "{}", res.text());
                    let auto = res.json::<Value>()["id"].as_str().unwrap().to_owned();
                    let runs = || async {
                        let res = s
                            .get(&format!("/v1/automations/{auto}/runs"))
                            .add_header("cookie", &cookie)
                            .await;
                        res.assert_status_ok();
                        res.json::<Value>()["runs"].as_array().unwrap().clone()
                    };
                    // Runs are stored `running` until every action was tried.
                    let settled = |r: &Vec<Value>, n: usize| {
                        r.len() == n && r.iter().all(|run| run["status"] != "running")
                    };
                    // A moment ahead, so the comment is after the automation was turned on.
                    let when = now() + 5;

                    // The private reply goes to the comment, not to a user id.
                    deliver(&s, &page_comment("101_9_501", "guide please", "777", when)).await;
                    let first =
                        until(|| async { Some(runs().await).filter(|r| settled(r, 1)) }).await;
                    assert_eq!(first[0]["status"], "sent", "{}", first[0]);
                    let calls = calls_since(0)
                        .into_iter()
                        .filter(|(path, _)| path == "101/messages")
                        .collect::<Vec<_>>();
                    assert_eq!(calls.len(), 1);
                    assert_eq!(calls[0].1["recipient"], r#"{"comment_id":"101_9_501"}"#);
                    assert_eq!(
                        calls[0].1["message"],
                        r#"{"text":"Here is the guide: https://x.io"}"#
                    );
                    assert!(!calls[0].1.contains_key("messaging_type"));

                    // Meta takes one private reply per comment: a second never goes out.
                    let fb = account(&db, &team_id, "facebook", "101").await;
                    let item = stored(&db, &team_id).await.remove(0);
                    let again = inbox::send(&state, &fb, &item, Via::Dm, "again", &[], None, None)
                        .await
                        .unwrap();
                    assert_eq!(again.unwrap_err().code(), "already_replied");

                    // A repeated delivery runs nothing.
                    deliver(&s, &page_comment("101_9_501", "guide please", "777", when)).await;
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    assert_eq!(runs().await.len(), 1);

                    // Our own comment is not answered.
                    deliver(&s, &page_comment("101_9_502", "see the guide", "101", when)).await;
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    assert_eq!(runs().await.len(), 1);

                    // A commenter Meta cannot message: the run is skipped and the public
                    // fallback goes under the comment.
                    deliver(&s, &page_comment("101_9_666", "guide me", "888", when)).await;
                    let both =
                        until(|| async { Some(runs().await).filter(|r| settled(r, 2)) }).await;
                    let skipped = both
                        .iter()
                        .find(|r| r["reason"] == "dm_unavailable")
                        .unwrap();
                    assert_eq!(skipped["status"], "sent", "the fallback went: {skipped}");
                    let results = skipped["results"].as_array().unwrap();
                    assert_eq!(results[0]["status"], "skipped");
                    assert_eq!(results[1]["type"], "fallback_public_reply");
                    assert_eq!(results[1]["status"], "sent");
                    let fallback = calls_since(0)
                        .into_iter()
                        .find(|(path, _)| path == "101_9_666/comments")
                        .unwrap();
                    assert_eq!(fallback.1["message"], "Please check your DMs");
                }

                async fn guide_automation(s: &TestServer, cookie: &str) -> String {
                    let res = req(s, Method::POST, "/v1/automations", cookie)
                        .json(&json!({
                            "name": "Guide by DM", "trigger": "comment_received",
                            "conditions": {},
                            "actions": [{"type": "send_dm", "text": "Here is the guide"}],
                        }))
                        .await;
                    assert_eq!(res.status_code(), 201, "{}", res.text());
                    res.json::<Value>()["id"].as_str().unwrap().to_owned()
                }

                async fn runs_of(s: &TestServer, cookie: &str, auto: &str) -> Vec<Value> {
                    let res = s
                        .get(&format!("/v1/automations/{auto}/runs"))
                        .add_header("cookie", cookie)
                        .await;
                    res.assert_status_ok();
                    res.json::<Value>()["runs"].as_array().unwrap().clone()
                }

                #[tokio::test]
                #[serial]
                async fn items_stored_before_a_database_error_still_run_their_automations() {
                    let (s, db, _, _) = on_meta(true).await;
                    clean(&db).await;
                    let (cookie, team_id) = owner_connected(&s, &db).await;
                    let auto = guide_automation(&s, &cookie).await;
                    let when = now() + 5;
                    // Postgres refuses a NUL in text: the second entry fails once the first
                    // is stored.
                    let good = page_comment("101_9_801", "guide please", "777", when);
                    let bad = page_comment("101_9_802", "guide\u{0}", "888", when);
                    let both = json!({"object": "page",
                        "entry": [good["entry"][0].clone(), bad["entry"][0].clone()]});
                    assert_eq!(deliver(&s, &both).await.status_code(), 500);
                    let items = stored(&db, &team_id).await;
                    assert_eq!(items.len(), 1);
                    assert_eq!(items[0].remote_id, "101_9_801");
                    // Its automation ran although the delivery failed...
                    let runs = until(|| async {
                        let r = runs_of(&s, &cookie, &auto).await;
                        Some(r).filter(|r| r.len() == 1 && r[0]["status"] != "running")
                    })
                    .await;
                    assert_eq!(runs[0]["status"], "sent", "{}", runs[0]);
                    // ...and Meta's redelivery, which finds it stored, does not run it again.
                    assert_eq!(deliver(&s, &both).await.status_code(), 500);
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    assert_eq!(runs_of(&s, &cookie, &auto).await.len(), 1);
                    let sent = calls_since(0)
                        .into_iter()
                        .filter(|(path, _)| path == "101/messages")
                        .count();
                    assert_eq!(sent, 1);
                }

                #[tokio::test]
                #[serial]
                async fn the_accounts_own_instagram_comments_are_skipped_whatever_id_they_carry() {
                    let (s, db, _, _) = on_meta(true).await;
                    clean(&db).await;
                    let (cookie, team_id) = owner_connected(&s, &db).await;
                    let auto = guide_automation(&s, &cookie).await;
                    // Instagram names the commenter by an id that is not the entry's, but
                    // the username is the account's (case aside).
                    for (id, username) in [("2001", "cafe"), ("2002", "Cafe")] {
                        let own = ig_comment_as("1784", id, "17841400000000", username);
                        assert_eq!(deliver(&s, &own).await.status_code(), 200);
                    }
                    assert!(stored(&db, &team_id).await.is_empty());

                    // A reply we sent comes back as a comment of someone unrecognisable; its
                    // id gives it away.
                    assert_eq!(
                        deliver(&s, &ig_comment("1784", "1790", "555"))
                            .await
                            .status_code(),
                        200
                    );
                    let first = stored(&db, &team_id).await.remove(0);
                    let res = req(
                        &s,
                        Method::POST,
                        &format!("/v1/inbox-items/{}/reply", first.id),
                        &cookie,
                    )
                    .json(&json!({"text": "Thanks!"}))
                    .await;
                    assert_eq!(res.status_code(), 201, "{}", res.text());
                    let sent = res.json::<Value>()["replies"][0]["remote_id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    let echo = ig_comment_as("1784", &sent, "17841400000001", "someone_else");
                    assert_eq!(deliver(&s, &echo).await.status_code(), 200);
                    assert_eq!(stored(&db, &team_id).await.len(), 1);
                    // Only the real comment ever ran the automation.
                    let runs = until(|| async {
                        let r = runs_of(&s, &cookie, &auto).await;
                        Some(r).filter(|r| r.iter().all(|run| run["status"] != "running"))
                    })
                    .await;
                    assert!(runs.len() <= 1);
                }
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
            #[tokio::test]
            #[serial]
            async fn post_groups_take_tokens_keys_and_mcp() {
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
                let set = make_token(&s, &owner, json!({"name": "cm", "social_set_id": clients}))
                    .await["secret"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let team = make_token(&s, &owner, json!({"name": "team"})).await["secret"]
                    .as_str()
                    .unwrap()
                    .to_owned();

                // A set token writes only to its set, which has no accounts here.
                let res = bearer(&s, Method::POST, "/v1/post-groups", &set)
                    .json(&json!({"social_set_id": default, "body": "hi"}))
                    .await;
                res.assert_json(&json!({"error": "invalid_set"}));
                let res = bearer(&s, Method::POST, "/v1/post-groups", &set)
                    .json(&json!({"platforms": ["bluesky"], "body": "mine"}))
                    .await;
                assert_eq!(res.status_code(), 201, "{}", res.text());
                assert_eq!(
                    res.json::<Value>()["posts"][0]["social_set_id"],
                    clients.as_str()
                );

                // An Idempotency-Key replays the same group.
                let body = json!({"social_set_id": default, "body": "once"});
                let send = || {
                    bearer(&s, Method::POST, "/v1/post-groups", &team)
                        .add_header("idempotency-key", "group-1")
                        .json(&body)
                };
                let first = send().await;
                assert_eq!(first.status_code(), 201, "{}", first.text());
                let again = send().await;
                assert_eq!(again.header("idempotent-replayed"), "true");
                assert_eq!(again.text(), first.text());
                let gid = first.json::<Value>()["group_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let path = format!("/v1/posts?group_id={gid}");
                let rest = bearer(&s, Method::GET, &path, &team).await.json::<Value>();
                assert_eq!(rest.as_array().unwrap().len(), 1);

                // MCP: the same service, and list-posts takes the group filter.
                let args = json!({"social_set_id": default, "body": "via mcp"});
                let (err, g) = mcp::call(&s, &team, "create-post-group", args).await;
                assert!(!err, "{g}");
                assert_eq!(g["posts"][0]["platform"], "bluesky");
                let gid = g["group_id"].as_str().unwrap();
                let (_, list) = mcp::call(&s, &team, "list-posts", json!({"group_id": gid})).await;
                assert_eq!(list, g["posts"]);
                let args = json!({"social_set_id": default, "body": "x",
                    "overrides": {"x": {}}});
                let (err, e) = mcp::call(&s, &team, "create-post-group", args).await;
                assert!(err);
                assert_eq!(e, json!({"error": "invalid_override"}));
            }

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
                            "create-post-group",
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
                    let tools = body["result"]["tools"].as_array().unwrap();
                    for name in ["create-post", "update-post", "create-post-group"] {
                        let tool = tools.iter().find(|t| t["name"] == name).unwrap();
                        assert!(
                            tool["inputSchema"]["properties"]["thread"].is_object(),
                            "{name}: {tool}"
                        );
                    }

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

                /// Over 64 KiB is 413, with a length header or chunked without one; a post group
                /// well past 4 KiB goes through.
                #[tokio::test]
                #[serial]
                async fn mcp_bodies_over_64_kib_are_refused() {
                    use std::io::{Read, Write};
                    let (s, db) = on_mock(120).await;
                    let (_, owner, _) = user(&s, &db).await;
                    let made = make_token(&s, &owner, json!({"name": "m"})).await;
                    let secret = made["secret"].as_str().unwrap().to_owned();
                    let team_id = current(&s, &owner).await["id"].as_str().unwrap().to_owned();
                    let set = default_set(&db, &team_id).await.to_string();
                    let media = |n: usize| -> Vec<String> {
                        (0..4)
                            .map(|i| format!("https://a.example/{n}{i}/{}", "p".repeat(2000)))
                            .collect()
                    };
                    let args = json!({"social_set_id": set, "platforms": ["bluesky"],
                        "body": "\u{1F600}".repeat(300), "media": media(0),
                        "overrides": {"bluesky": {"media": media(1)}}});
                    assert!(args.to_string().len() > 10 * 1024);
                    let (err, g) = call(&s, &secret, "create-post-group", args).await;
                    assert!(!err, "{g}");
                    assert_eq!(g["posts"][0]["media"], json!(media(1)));

                    let big = json!({"name": "create-post",
                        "arguments": {"body": "a".repeat(70 * 1024)}});
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
                    let big = post_body(Some(&set), &"x".repeat(65 * 1024));
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
            async fn target_events_carry_the_group_id() {
                let (s, db, _) = on_mock().await;
                let hook = receiver().await;
                let (cookie, team_id, _) = owner_with_account(&s, &db).await;
                let set = default_set(&db, &team_id).await.to_string();
                assert_eq!(
                    put(&s, &cookie, &format!("{hook}/hook"))
                        .await
                        .status_code(),
                    201
                );
                let made = req(&s, Method::POST, "/v1/post-groups", &cookie)
                    .json(&json!({"social_set_id": set, "body": "grouped"}))
                    .await
                    .json::<Value>();
                let id = made["posts"][0]["id"].as_str().unwrap().to_owned();
                let path = format!("/v1/posts/{id}/publish");
                assert_eq!(
                    req(&s, Method::POST, &path, &cookie).await.status_code(),
                    202
                );
                for _ in 0..100 {
                    if !deliveries(&db, &team_id).await.is_empty() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                let got = deliveries(&db, &team_id).await;
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].1["data"]["post_id"], id.as_str());
                assert_eq!(got[0].1["data"]["group_id"], made["group_id"]);
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

/// Turnstile on the public forms and `POST /v1/contact`, against a local fake siteverify (it
/// passes the token `good` sent with the secret `sec`) and a fake Resend.
mod captcha_and_contact {
    use super::*;
    use axum::{routing::post, Form, Json, Router};
    use sea_orm::DatabaseConnection;
    use socialreins_brain::{mail::Mailer, turnstile::Turnstile};
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    pub(super) type Seen = Arc<Mutex<Vec<Value>>>;
    const TO: &str = "inbox@example.com";

    pub(super) async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        base
    }

    async fn siteverify() -> String {
        let verify = |Form(f): Form<HashMap<String, String>>| async move {
            let get = |k: &str| f.get(k).map(String::as_str);
            Json(
                json!({"success": get("secret") == Some("sec") && get("response") == Some("good")}),
            )
        };
        serve(Router::new().route("/v", post(verify))).await + "/v"
    }

    /// A port nothing listens on.
    async fn down() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v", listener.local_addr().unwrap());
        drop(listener);
        url
    }

    pub(super) async fn resend() -> (String, Seen) {
        let seen = Seen::default();
        let log = seen.clone();
        let app = Router::new().route(
            "/emails",
            post(move |Json(body): Json<Value>| async move {
                log.lock().unwrap().push(body);
                Json(json!({"id": "x"}))
            }),
        );
        (serve(app).await + "/emails", seen)
    }

    /// Turnstile verifying at `verify` (off when `None`), contact mail to `TO` through a fake
    /// Resend, and `X-Forwarded-For` trusted so tests can pick the client IP.
    async fn app(verify: Option<String>) -> (TestServer, DatabaseConnection, Seen) {
        let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
        config.trust_proxy = true;
        config.turnstile = verify.map(|url| {
            let mut t = Turnstile::new("site", "sec").unwrap();
            t.verify_url = url;
            t
        });
        config.contact_to = Some(TO.into());
        let (url, seen) = resend().await;
        config.mailer = Mailer::resend_at(&url, "re_test", "brain@example.com").unwrap();
        let db = connect(&config).await.expect("database reachable");
        let app = controllers::router(AppState::new(db.clone(), &config), &config);
        (TestServer::new(app).unwrap(), db, seen)
    }

    fn contact_body(token: Option<&str>) -> Value {
        json!({"name": "Jane", "email": unique_email(), "message": "Hi", "turnstile_token": token})
    }

    /// Each form with `token`: its path, body and success status. The waitlist has no captcha.
    fn forms(token: Option<&str>) -> [(&'static str, Value, u16); 2] {
        let email = json!({"email": unique_email(), "turnstile_token": token});
        [
            ("/auth/link", email, 202),
            ("/v1/contact", contact_body(token), 202),
        ]
    }

    #[tokio::test]
    #[serial]
    async fn waitlist_ignores_turnstile() {
        let (s, db, _) = app(Some(siteverify().await)).await;
        for token in [None, Some("bad")] {
            let email = unique_email();
            let body = json!({"email": email, "turnstile_token": token});
            s.post("/v1/waitlist").json(&body).await.assert_status_ok();
            assert_eq!(rows(&db, &email).await, 1);
        }
    }

    #[tokio::test]
    #[serial]
    async fn providers_give_the_site_key_or_null() {
        let (s, _, _) = app(Some(siteverify().await)).await;
        let on = s.get("/auth/providers").await.json::<Value>();
        assert_eq!(on["turnstile_site_key"], "site");
        let (s, _, _) = app(None).await;
        let off = s.get("/auth/providers").await.json::<Value>();
        assert_eq!(off["turnstile_site_key"], Value::Null);
    }

    #[tokio::test]
    #[serial]
    async fn forms_need_a_good_token_when_turnstile_is_on() {
        let (s, _, seen) = app(Some(siteverify().await)).await;
        for token in [None, Some(""), Some("bad")] {
            for (path, body, _) in forms(token) {
                let res = s.post(path).json(&body).await;
                assert_eq!(res.status_code(), 422, "{path} {token:?}");
                res.assert_json(&json!({"error": "captcha_failed"}));
            }
        }
        assert!(
            seen.lock().unwrap().is_empty(),
            "no mail before the captcha"
        );
        let (s, _, _) = app(Some(siteverify().await)).await;
        for (path, body, status) in forms(Some("good")) {
            let res = s.post(path).json(&body).await;
            assert_eq!(res.status_code(), status, "{path}");
        }
    }

    #[tokio::test]
    #[serial]
    async fn forms_fail_closed_when_siteverify_is_down() {
        let (s, _, _) = app(Some(down().await)).await;
        for (path, body, _) in forms(Some("good")) {
            let res = s.post(path).json(&body).await;
            assert_eq!(res.status_code(), 503, "{path}");
            res.assert_json(&json!({"error": "captcha_unavailable"}));
        }
    }

    #[tokio::test]
    #[serial]
    async fn forms_ignore_the_token_when_turnstile_is_off() {
        let (s, _, _) = app(None).await;
        for token in [None, Some("anything")] {
            for (path, body, status) in forms(token) {
                let res = s.post(path).json(&body).await;
                assert_eq!(res.status_code(), status, "{path} {token:?}");
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn contact_mails_contact_to_with_reply_to() {
        let (s, _, seen) = app(None).await;
        let message = "é".repeat(5000);
        let body = json!({"name": "  Jane Doe ", "email": "Jane@Example.com", "message": message});
        let res = s.post("/v1/contact").json(&body).await;
        assert_eq!(res.status_code(), 202);
        res.assert_json(&json!({}));
        let mail = seen.lock().unwrap().pop().expect("mail sent");
        assert_eq!(mail["to"], json!([TO]));
        assert_eq!(mail["reply_to"], "jane@example.com");
        assert_eq!(mail["subject"], "Contact form: Jane Doe");
        let text = mail["text"].as_str().unwrap();
        assert!(text.contains("Jane Doe") && text.contains("jane@example.com"));
        assert!(text.contains(&message));
    }

    #[tokio::test]
    #[serial]
    async fn contact_rejects_bad_fields() {
        let (s, _, seen) = app(None).await;
        let long_name = "a".repeat(101);
        let long_message = "a".repeat(5001);
        let cases = [
            (
                json!({"name": " ", "email": "a@example.com", "message": "Hi"}),
                "invalid_name",
            ),
            (
                json!({"name": long_name, "email": "a@example.com", "message": "Hi"}),
                "invalid_name",
            ),
            (
                json!({"name": "A\r\nBcc: x@evil.com", "email": "a@example.com", "message": "Hi"}),
                "invalid_name",
            ),
            (
                json!({"name": "A", "email": "nope", "message": "Hi"}),
                "invalid_email",
            ),
            (
                json!({"name": "A", "email": "a@example.com\r\nBcc: x@evil.com", "message": "Hi"}),
                "invalid_email",
            ),
            (
                json!({"name": "A", "email": "a@example.com\n", "message": "Hi"}),
                "invalid_email",
            ),
            (
                json!({"name": "A", "email": "a@example.com", "message": "  "}),
                "invalid_message",
            ),
            (
                json!({"name": "A", "email": "a@example.com", "message": long_message}),
                "invalid_message",
            ),
        ];
        for (i, (body, code)) in cases.into_iter().enumerate() {
            let res = s
                .post("/v1/contact")
                .add_header("x-forwarded-for", format!("198.51.100.{i}"))
                .json(&body)
                .await;
            assert_eq!(res.status_code(), 422, "{code} {body}");
            res.assert_json(&json!({"error": code}));
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    #[serial]
    async fn contact_is_off_without_contact_to() {
        let mut config = Config::from_env().unwrap();
        config.contact_to = None;
        let db = connect(&config).await.unwrap();
        let app = controllers::router(AppState::new(db, &config), &config);
        let res = TestServer::new(app)
            .unwrap()
            .post("/v1/contact")
            .json(&contact_body(None))
            .await;
        assert_eq!(res.status_code(), 503);
        res.assert_json(&json!({"error": "contact_unavailable"}));
    }

    #[tokio::test]
    #[serial]
    async fn contact_allows_5_an_hour_per_ip() {
        let (s, _, _) = app(None).await;
        let post = |ip: &'static str| {
            s.post("/v1/contact")
                .add_header("x-forwarded-for", ip)
                .json(&contact_body(None))
        };
        for _ in 0..5 {
            assert_eq!(post("203.0.113.20").await.status_code(), 202);
        }
        let res = post("203.0.113.20").await;
        assert_eq!(res.status_code(), 429);
        res.assert_json(&json!({"error": "rate_limited"}));
        assert_eq!(post("203.0.113.21").await.status_code(), 202);
    }

    #[tokio::test]
    #[serial]
    async fn openapi_documents_contact() {
        let (s, _, _) = app(None).await;
        let doc = s.get("/openapi.json").await.json::<Value>();
        assert!(doc["paths"]["/contact"]["post"].is_object());
    }
}

/// Waitlist confirmation mail, the verify link, the public count and the admin list, with mail
/// going to a fake Resend.
mod waitlist_confirm {
    use super::captcha_and_contact::{resend, Seen};
    use super::*;
    use chrono::{Duration, Utc};
    use sea_orm::{sea_query::Expr, ConnectionTrait, DatabaseConnection};
    use socialreins_brain::mail::Mailer;

    const ADMIN: &str = "boss@example.com";

    /// Empties the global daily cap on confirmation emails, which other tests also use up.
    async fn reset_cap(db: &DatabaseConnection) {
        let sql = "DELETE FROM rate_limit_hits WHERE bucket LIKE 'waitlist:confirm%'";
        db.execute_unprepared(sql).await.unwrap();
    }

    async fn app() -> (TestServer, DatabaseConnection, Seen, String) {
        let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
        config.admin_emails = vec![ADMIN.into()];
        let (url, seen) = resend().await;
        config.mailer = Mailer::resend_at(&url, "re_test", "brain@example.com").unwrap();
        let web = config.web_url.clone();
        let db = connect(&config).await.expect("database reachable");
        reset_cap(&db).await;
        let app = controllers::router(AppState::new(db.clone(), &config), &config);
        (TestServer::new(app).unwrap(), db, seen, web)
    }

    /// The mails sent to `email` so far, after giving the background send a moment.
    async fn mails(seen: &Seen, email: &str) -> Vec<Value> {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let all = seen.lock().unwrap().clone();
        all.into_iter()
            .filter(|m| m["to"] == json!([email]))
            .collect()
    }

    fn token_of(mail: &Value) -> String {
        let text = mail["text"].as_str().unwrap();
        let (_, rest) = text.split_once("/waitlist/verify?token=").unwrap();
        rest.split_whitespace().next().unwrap().to_owned()
    }

    async fn row(db: &DatabaseConnection, email: &str) -> waitlist_signups::Model {
        waitlist_signups::Entity::find()
            .filter(waitlist_signups::Column::Email.eq(email))
            .one(db)
            .await
            .unwrap()
            .unwrap()
    }

    /// Moves the last confirmation email of `email` back by `by`.
    async fn backdate_sent(db: &DatabaseConnection, email: &str, by: Duration) {
        waitlist_signups::Entity::update_many()
            .col_expr(
                waitlist_signups::Column::VerifySentAt,
                Expr::value((Utc::now() - by).fixed_offset()),
            )
            .filter(waitlist_signups::Column::Email.eq(email))
            .exec(db)
            .await
            .unwrap();
    }

    async fn join(s: &TestServer, email: &str) {
        let res = s.post("/v1/waitlist").json(&json!({"email": email})).await;
        res.assert_status_ok();
        res.assert_json(&json!({"status": "ok"}));
    }

    /// Posts `token` the way the web page does: `true` when verified, `false` for `invalid_link`.
    async fn verify(s: &TestServer, token: &str) -> bool {
        let res = s
            .post("/v1/waitlist/verify")
            .json(&json!({"token": token}))
            .await;
        if res.status_code() == 200 {
            res.assert_json(&json!({"status": "verified"}));
            return true;
        }
        assert_eq!(res.status_code(), 422, "{token}");
        res.assert_json(&json!({"error": "invalid_link"}));
        false
    }

    #[tokio::test]
    #[serial]
    async fn join_mails_a_confirmation_link() {
        let (s, db, seen, web) = app().await;
        let email = unique_email();
        join(&s, &email).await;
        let sent = mails(&seen, &email).await;
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0]["subject"],
            "Confirm your spot on the SocialReins waitlist"
        );
        let text = sent[0]["text"].as_str().unwrap();
        assert!(text.contains("Confirm my email"), "{text}");
        assert!(
            text.contains(&format!("{web}/waitlist/verify?token=")),
            "{text}"
        );
        let token = token_of(&sent[0]);
        let r = row(&db, &email).await;
        assert_eq!(
            r.verify_token_hash,
            Some(socialreins_core::token::hash(&token))
        );
        assert!(r.verify_sent_at.is_some() && r.verified_at.is_none());
    }

    #[tokio::test]
    #[serial]
    async fn resends_at_most_hourly_and_never_after_verifying() {
        let (s, db, seen, _) = app().await;
        let email = unique_email();
        join(&s, &email).await;
        join(&s, &email).await;
        assert_eq!(mails(&seen, &email).await.len(), 1, "throttled");
        backdate_sent(&db, &email, Duration::minutes(61)).await;
        join(&s, &email).await;
        let sent = mails(&seen, &email).await;
        assert_eq!(sent.len(), 2, "resent after an hour");
        let (old, new) = (token_of(&sent[0]), token_of(&sent[1]));
        assert_ne!(old, new);
        assert!(!verify(&s, &old).await, "replaced token");
        assert!(verify(&s, &new).await);
        backdate_sent(&db, &email, Duration::hours(2)).await;
        join(&s, &email).await;
        assert_eq!(mails(&seen, &email).await.len(), 2, "verified gets nothing");
    }

    #[tokio::test]
    #[serial]
    async fn mail_failure_still_answers_ok_and_allows_a_retry() {
        let mut config = Config::from_env().unwrap();
        config.mailer =
            Mailer::resend_at("http://127.0.0.1:9/emails", "k", "a@example.com").unwrap();
        let db = connect(&config).await.unwrap();
        reset_cap(&db).await;
        let s = TestServer::new(controllers::router(
            AppState::new(db.clone(), &config),
            &config,
        ))
        .unwrap();
        let email = unique_email();
        join(&s, &email).await;
        assert_eq!(rows(&db, &email).await, 1);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let r = row(&db, &email).await;
        assert!(
            r.verify_sent_at.is_none(),
            "a failed send does not throttle"
        );
    }

    #[tokio::test]
    #[serial]
    async fn one_ip_gets_20_confirmations_a_day() {
        let mut config = Config::from_env().unwrap();
        config.trust_proxy = true;
        let (url, seen) = resend().await;
        config.mailer = Mailer::resend_at(&url, "re_test", "brain@example.com").unwrap();
        let db = connect(&config).await.unwrap();
        reset_cap(&db).await;
        let s = TestServer::new(controllers::router(
            AppState::new(db.clone(), &config),
            &config,
        ))
        .unwrap();
        let fill = "INSERT INTO rate_limit_hits (bucket) \
                    SELECT 'waitlist:confirm:ip:203.0.113.50' FROM generate_series(1, 20)";
        db.execute_unprepared(fill).await.unwrap();
        let join_from = |ip: &'static str, email: String| {
            s.post("/v1/waitlist")
                .add_header("x-forwarded-for", ip)
                .json(&json!({"email": email}))
        };
        let (capped, other) = (unique_email(), unique_email());
        join_from("203.0.113.50", capped.clone())
            .await
            .assert_status_ok();
        join_from("203.0.113.51", other.clone())
            .await
            .assert_status_ok();
        assert!(mails(&seen, &capped).await.is_empty(), "IP over its cap");
        assert_eq!(rows(&db, &capped).await, 1, "signup still saved");
        assert_eq!(mails(&seen, &other).await.len(), 1, "other IPs unaffected");
        let global = "SELECT count(*) AS n FROM rate_limit_hits WHERE bucket = 'waitlist:confirm'";
        let n: i64 = db
            .query_one_raw(sea_orm::Statement::from_string(
                db.get_database_backend(),
                global,
            ))
            .await
            .unwrap()
            .unwrap()
            .try_get("", "n")
            .unwrap();
        assert_eq!(n, 1, "the refused IP used none of the global allowance");
    }

    #[tokio::test]
    #[serial]
    async fn over_the_daily_cap_saves_but_does_not_mail() {
        let (s, db, seen, _) = app().await;
        let fill = "INSERT INTO rate_limit_hits (bucket) \
                    SELECT 'waitlist:confirm' FROM generate_series(1, 500)";
        db.execute_unprepared(fill).await.unwrap();
        let email = unique_email();
        join(&s, &email).await;
        assert!(mails(&seen, &email).await.is_empty());
        assert_eq!(rows(&db, &email).await, 1);
        assert!(row(&db, &email).await.verify_sent_at.is_none());
        reset_cap(&db).await;
        join(&s, &email).await;
        assert_eq!(
            mails(&seen, &email).await.len(),
            1,
            "sent once the cap frees up"
        );
    }

    #[tokio::test]
    #[serial]
    async fn verify_marks_once() {
        let (s, db, seen, _) = app().await;
        let email = unique_email();
        join(&s, &email).await;
        let token = token_of(&mails(&seen, &email).await[0]);
        assert!(verify(&s, &token).await);
        let first = row(&db, &email).await.verified_at.expect("verified");
        assert!(verify(&s, &token).await, "a second click is fine");
        assert_eq!(row(&db, &email).await.verified_at, Some(first));
        for token in ["", "abc", &"0".repeat(64), &format!("{token}0")] {
            assert!(!verify(&s, token).await, "{token}");
        }
        let res = s.post("/v1/waitlist/verify").json(&json!({})).await;
        assert_eq!(res.status_code(), 422);
        res.assert_json(&json!({"error": "invalid_link"}));
        let get = s.get(&format!("/v1/waitlist/verify?token={token}")).await;
        assert_eq!(get.status_code(), 405, "a GET changes nothing");
    }

    #[tokio::test]
    #[serial]
    async fn verify_shares_the_waitlist_rate_limit() {
        let (s, _, _, _) = app().await;
        for _ in 0..10 {
            verify(&s, &"0".repeat(64)).await;
        }
        let res = s
            .post("/v1/waitlist/verify")
            .json(&json!({"token": "x"}))
            .await;
        assert_eq!(res.status_code(), 429);
    }

    #[tokio::test]
    #[serial]
    async fn count_is_every_signup_cached() {
        let (s, db, _, _) = app().await;
        let res = s.get("/v1/waitlist/count").await;
        res.assert_status_ok();
        let all = waitlist_signups::Entity::find().count(&db).await.unwrap();
        res.assert_json(&json!({"count": all}));
        join(&s, &unique_email()).await;
        let again = s.get("/v1/waitlist/count").await.json::<Value>();
        assert_eq!(again["count"], all, "served from the 30 second cache");
        let (fresh, _, _, _) = app().await;
        let now = fresh.get("/v1/waitlist/count").await.json::<Value>();
        assert_eq!(now["count"], all + 1);
    }

    /// A session cookie for `email`, signed in by magic link.
    async fn signed_in(s: &TestServer, db: &DatabaseConnection, email: &str) -> String {
        let secret = auth::link(db, email, Duration::minutes(15)).await;
        let (_, cookie) = auth::open(s, &secret).await;
        auth::session(&cookie.unwrap())
    }

    #[tokio::test]
    #[serial]
    async fn admin_list_needs_an_admin_session() {
        let (s, db, seen, _) = app().await;
        let res = s.get("/v1/admin/waitlist").await;
        assert_eq!(res.status_code(), 401);
        res.assert_json(&json!({"error": "unauthorized"}));
        let user = signed_in(&s, &db, &unique_email()).await;
        let res = s
            .get("/v1/admin/waitlist")
            .add_header("cookie", &user)
            .await;
        assert_eq!(res.status_code(), 403);
        res.assert_json(&json!({"error": "forbidden"}));
        let me = s.get("/auth/me").add_header("cookie", &user).await;
        assert_eq!(me.json::<Value>()["is_admin"], false);

        let (yes, no) = (unique_email(), unique_email());
        join(&s, &yes).await;
        join(&s, &no).await;
        verify(&s, &token_of(&mails(&seen, &yes).await[0])).await;
        let admin = signed_in(&s, &db, ADMIN).await;
        let me = s.get("/auth/me").add_header("cookie", &admin).await;
        assert_eq!(me.json::<Value>()["is_admin"], true);
        let list = |q: &'static str| {
            s.get(&format!("/v1/admin/waitlist{q}"))
                .add_header("cookie", &admin)
        };
        let emails = |body: &Value| -> Vec<String> {
            let rows = body["signups"].as_array().unwrap();
            rows.iter()
                .map(|r| r["email"].as_str().unwrap().to_owned())
                .collect()
        };
        let all = list("").await;
        all.assert_status_ok();
        let all = all.json::<Value>();
        let total = waitlist_signups::Entity::find().count(&db).await.unwrap();
        let verified = waitlist_signups::Entity::find()
            .filter(waitlist_signups::Column::VerifiedAt.is_not_null())
            .count(&db)
            .await
            .unwrap();
        assert_eq!(
            (all["total"].as_u64(), all["verified"].as_u64()),
            (Some(total), Some(verified))
        );
        assert_eq!(emails(&all)[..2], [no.clone(), yes.clone()], "newest first");
        let first = &all["signups"][0];
        for key in ["id", "email", "source", "created_at", "verified_at"] {
            assert!(first.get(key).is_some(), "{key}");
        }
        let only_yes = list("?verified=true").await.json::<Value>();
        assert!(emails(&only_yes).contains(&yes) && !emails(&only_yes).contains(&no));
        assert_eq!(
            only_yes["total"].as_u64(),
            Some(total),
            "totals stay overall"
        );
        let only_no = list("?verified=false").await.json::<Value>();
        assert!(emails(&only_no).contains(&no) && !emails(&only_no).contains(&yes));
        assert_eq!(list("?verified=maybe").await.status_code(), 422);
    }

    #[tokio::test]
    #[serial]
    async fn openapi_documents_the_new_routes() {
        let (s, _, _, _) = app().await;
        let doc = s.get("/openapi.json").await.json::<Value>();
        for (path, method) in [
            ("/waitlist/verify", "post"),
            ("/waitlist/count", "get"),
            ("/admin/waitlist", "get"),
        ] {
            assert!(doc["paths"][path][method].is_object(), "{path}");
        }
    }
}
