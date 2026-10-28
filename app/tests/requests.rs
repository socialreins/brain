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
    async fn only_failed_logins_count_per_email() {
        let (s, db) = server_where(|c| c.trust_proxy = true).await;
        let (email, cookie) = signed_in(&s, &db).await;
        set_password(&s, &cookie, json!({"password": PASSWORD}))
            .await
            .assert_status(axum::http::StatusCode::NO_CONTENT);
        let from = |ip: &str, pw: &str| {
            s.post("/auth/login")
                .add_header("origin", WEB)
                .add_header("x-forwarded-for", ip.to_owned())
                .json(&json!({"email": email, "password": pw}))
        };
        // Another IP's failures under the cap do not stop the owner, and a success clears them.
        for _ in 0..4 {
            assert_eq!(
                from("198.51.100.1", "wrong guess here").await.status_code(),
                401
            );
        }
        from("198.51.100.2", PASSWORD).await.assert_status_ok();
        for _ in 0..4 {
            assert_eq!(
                from("198.51.100.3", "wrong guess here").await.status_code(),
                401
            );
        }
        from("198.51.100.2", PASSWORD).await.assert_status_ok();
        // Five failures in a row, from any IPs, close the email for the window.
        for i in 0..5 {
            let ip = format!("198.51.100.{}", 10 + i);
            assert_eq!(from(&ip, "wrong guess here").await.status_code(), 401);
        }
        let res = from("198.51.100.20", PASSWORD).await;
        assert_eq!(res.status_code(), 429);
        res.assert_json(&json!({"error": "rate_limited"}));
    }

    #[tokio::test]
    #[serial]
    async fn concurrent_wrong_guesses_cannot_pass_the_cap() {
        let (s, db) = server_where(|c| c.trust_proxy = true).await;
        let (email, cookie) = signed_in(&s, &db).await;
        set_password(&s, &cookie, json!({"password": PASSWORD}))
            .await
            .assert_status(axum::http::StatusCode::NO_CONTENT);
        let guess = |i: u8| {
            s.post("/auth/login")
                .add_header("origin", WEB)
                .add_header("x-forwarded-for", format!("198.51.100.{i}"))
                .json(&json!({"email": email, "password": "wrong guess here"}))
        };
        // Argon2 runs on blocking threads, so these verify in parallel.
        let res = tokio::join!(
            guess(1),
            guess(2),
            guess(3),
            guess(4),
            guess(5),
            guess(6),
            guess(7),
            guess(8)
        );
        let codes = [res.0, res.1, res.2, res.3, res.4, res.5, res.6, res.7]
            .map(|r| r.status_code().as_u16());
        assert_eq!(codes.iter().filter(|&&c| c == 401).count(), 5, "{codes:?}");
        assert_eq!(codes.iter().filter(|&&c| c == 429).count(), 3, "{codes:?}");
        // Outsiders filling the email's login budget do not block the signed-in owner.
        let res = set_password(
            &s,
            &cookie,
            json!({"password": "another long password", "current_password": PASSWORD}),
        )
        .await;
        assert_eq!(res.status_code(), 204);
    }

    #[tokio::test]
    #[serial]
    async fn a_session_cannot_guess_the_current_password_faster_than_its_cap() {
        let (s, db) = server_where(|_| {}).await;
        let (email, cookie) = signed_in(&s, &db).await;
        set_password(&s, &cookie, json!({"password": PASSWORD}))
            .await
            .assert_status(axum::http::StatusCode::NO_CONTENT);
        let change = |current: &str| {
            set_password(
                &s,
                &cookie,
                json!({"password": "another long password", "current_password": current}),
            )
        };
        for _ in 0..5 {
            assert_eq!(change("wrong guess here").await.status_code(), 403);
        }
        assert_eq!(change(PASSWORD).await.status_code(), 429);
        // The failures were charged to the email's login budget as well.
        assert_eq!(login(&s, &email, PASSWORD).await.status_code(), 429);
    }

    #[tokio::test]
    #[serial]
    async fn setting_a_password_ends_other_sessions_and_links() {
        let (s, db) = server_where(|_| {}).await;
        let (email, cookie) = signed_in(&s, &db).await;
        let (_, other) = open(&s, &link(&db, &email, Duration::minutes(15)).await).await;
        let other = session(&other.unwrap());
        let me = |c: &str| s.get("/auth/me").add_header("cookie", c.to_owned());
        me(&other).await.assert_status_ok();
        let pending = link(&db, &email, Duration::minutes(15)).await;
        set_password(&s, &cookie, json!({"password": PASSWORD}))
            .await
            .assert_status(axum::http::StatusCode::NO_CONTENT);
        assert_eq!(me(&other).await.status_code(), 401);
        me(&cookie).await.assert_status_ok();
        assert_eq!(links(&db, &email).await, 0, "outstanding links are revoked");
        let (to, _) = open(&s, &pending).await;
        assert!(to.ends_with("/login?error=invalid_link"));
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

        // Deleting needs a recent sign-in (or the password).
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
}
