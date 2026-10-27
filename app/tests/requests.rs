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
        let redeem = s.post("/auth/magic").form(&[("token", "x")]).await;
        assert_eq!(redeem.status_code(), 429);
    }

    const WEB: &str = "http://localhost:3000";
    const PASSWORD: &str = "correct horse battery";

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

    async fn set_password(s: &TestServer, cookie: &str, body: Value) -> axum_test::TestResponse {
        s.put("/auth/password")
            .add_header("origin", WEB)
            .add_header("cookie", cookie)
            .json(&body)
            .await
    }

    async fn login(s: &TestServer, email: &str, password: &str) -> axum_test::TestResponse {
        s.post("/auth/login")
            .add_header("origin", WEB)
            .json(&json!({"email": email, "password": password}))
            .await
    }

    #[tokio::test]
    #[serial]
    async fn password_login_answers_one_way_for_every_failure() {
        let (s, db) = server_where(|_| {}).await;
        let (email, cookie) = signed_in(&s, &db).await;
        let (linkless, _) = signed_in(&s, &db).await;
        let short = set_password(&s, &cookie, json!({"password": "short"})).await;
        assert_eq!(short.status_code(), 422);
        short.assert_json(&json!({"error": "invalid_password"}));
        let set = set_password(&s, &cookie, json!({"password": PASSWORD})).await;
        assert_eq!(set.status_code(), 204);
        let me = s.get("/auth/me").add_header("cookie", &cookie).await;
        assert_eq!(me.json::<Value>()["has_password"], true);
        let stored = user(&db, &email).await.unwrap().password_hash.unwrap();
        assert!(stored.starts_with("$argon2id$"));

        for (who, pw) in [
            (email.as_str(), "wrong password here"),
            (&unique_email(), PASSWORD),
            (&linkless, PASSWORD),
        ] {
            let res = login(&s, who, pw).await;
            assert_eq!(res.status_code(), 401, "{who}");
            res.assert_json(&json!({"error": "invalid_credentials"}));
            assert!(res.maybe_header("set-cookie").is_none());
        }
        let ok = login(&s, &email.to_uppercase(), PASSWORD).await;
        ok.assert_status_ok();
        assert_eq!(ok.json::<Value>()["email"], email);
        let fresh = session(ok.header("set-cookie").to_str().unwrap());
        let me = s.get("/auth/me").add_header("cookie", fresh).await;
        me.assert_status_ok();
    }

    #[tokio::test]
    #[serial]
    async fn changing_a_password_needs_the_current_one() {
        let (s, db) = server_where(|_| {}).await;
        let (email, cookie) = signed_in(&s, &db).await;
        set_password(&s, &cookie, json!({"password": PASSWORD}))
            .await
            .assert_status(axum::http::StatusCode::NO_CONTENT);
        // An old session does not matter once a password exists: the current one is the check.
        age(&db, &cookie, Duration::hours(1)).await;
        let new = "a different long password";
        for current in [None, Some("not the password")] {
            let res = set_password(
                &s,
                &cookie,
                json!({"password": new, "current_password": current}),
            )
            .await;
            assert_eq!(res.status_code(), 403);
            res.assert_json(&json!({"error": "invalid_current_password"}));
        }
        let res = set_password(
            &s,
            &cookie,
            json!({"password": new, "current_password": PASSWORD}),
        )
        .await;
        assert_eq!(res.status_code(), 204);
        login(&s, &email, new).await.assert_status_ok();
    }

    #[tokio::test]
    #[serial]
    async fn first_password_and_passkey_need_a_recent_sign_in() {
        let (s, db) = server_where(|_| {}).await;
        let (email, cookie) = signed_in(&s, &db).await;
        age(&db, &cookie, Duration::minutes(11)).await;
        let res = set_password(&s, &cookie, json!({"password": PASSWORD})).await;
        assert_eq!(res.status_code(), 403);
        res.assert_json(&json!({"error": "reauth_required"}));
        let start = s
            .post("/auth/passkeys/register/start")
            .add_header("origin", WEB)
            .add_header("cookie", &cookie)
            .json(&json!({}))
            .await;
        assert_eq!(start.status_code(), 403);
        start.assert_json(&json!({"error": "reauth_required"}));
        assert!(user(&db, &email).await.unwrap().password_hash.is_none());
    }

    #[tokio::test]
    #[serial]
    async fn login_is_rate_limited_per_email_and_per_ip() {
        let (s, _) = server_where(|_| {}).await;
        let email = unique_email();
        for _ in 0..5 {
            assert_eq!(login(&s, &email, PASSWORD).await.status_code(), 401);
        }
        let res = login(&s, &email, PASSWORD).await;
        assert_eq!(res.status_code(), 429);
        res.assert_json(&json!({"error": "rate_limited"}));
        // Six attempts so far from this client; the per-IP cap of 10 stops new emails too.
        for _ in 0..4 {
            assert_eq!(
                login(&s, &unique_email(), PASSWORD).await.status_code(),
                401
            );
        }
        assert_eq!(
            login(&s, &unique_email(), PASSWORD).await.status_code(),
            429
        );
    }

    #[tokio::test]
    #[serial]
    async fn credential_endpoints_only_answer_the_web_origin() {
        let (s, db) = server_where(|_| {}).await;
        let (_, cookie) = signed_in(&s, &db).await;
        let body = json!({"email": unique_email(), "password": PASSWORD});
        for origin in [
            None,
            Some("https://evil.example"),
            Some("http://localhost:8080"),
        ] {
            let calls = [
                s.post("/auth/login").json(&body),
                s.put("/auth/password")
                    .add_header("cookie", &cookie)
                    .json(&json!({"password": PASSWORD})),
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
            .method(axum::http::Method::OPTIONS, "/auth/password")
            .add_header("origin", WEB)
            .add_header("access-control-request-method", "PUT")
            .await;
        assert!(preflight
            .header("access-control-allow-methods")
            .to_str()
            .unwrap()
            .contains("PUT"));
    }

    #[tokio::test]
    #[serial]
    async fn registration_disabled_does_not_change_password_login() {
        let (open_server, db) = server_where(|_| {}).await;
        let (email, cookie) = signed_in(&open_server, &db).await;
        set_password(&open_server, &cookie, json!({"password": PASSWORD}))
            .await
            .assert_status(axum::http::StatusCode::NO_CONTENT);
        let (s, _) = server_where(|c| c.registration_disabled = true).await;
        login(&s, &email, PASSWORD).await.assert_status_ok();
        let stranger = unique_email();
        assert_eq!(login(&s, &stranger, PASSWORD).await.status_code(), 401);
        assert!(user(&db, &stranger).await.is_none());
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

            let id = made["body"]["id"].as_str().unwrap();
            let remove = |c: &str| {
                s.delete(&format!("/auth/passkeys/{id}"))
                    .add_header("origin", WEB)
                    .add_header("cookie", c)
            };
            let res = remove(&stranger).await;
            assert_eq!(res.status_code(), 404);
            res.assert_json(&json!({"error": "not_found"}));
            assert_eq!(remove(&cookie).await.status_code(), 204);
            assert_eq!(remove(&cookie).await.status_code(), 404);
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
