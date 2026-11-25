//! The Threads inbox against a mock Threads (no real Threads call): what the poll stores and
//! skips, the first read and old items never being answered, replying, hiding, the errors and
//! the webhook hint. Needs `DATABASE_URL` like the request tests.
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::Duration as Wait,
};

use axum::{
    body::Bytes,
    extract::State,
    http::{Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json, Router,
};
use axum_test::TestServer;
use chrono::{Duration, Utc};
use sea_orm::{
    ActiveValue::Set, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait, Statement,
};
use serde_json::{json, Value};
use serial_test::serial;
use socialreins_brain::{
    config::Config, connect, connectors::threads::Threads, controllers, models::magic_links,
    state::AppState, threads_inbox,
};
use socialreins_core::token;
use uuid::Uuid;

const WEB: &str = "http://localhost:3000";

/// 0 works, 1 every read answers a dead token (190), 2 the replies are refused for a missing
/// permission (10), 3 every call is rate limited, 4 the mentions are refused (10), 5 a
/// reply is refused for a dead token.
#[derive(Default)]
struct Mock {
    mode: AtomicU8,
    /// (method, path, query, body)
    calls: Mutex<Vec<(String, String, String, String)>>,
    posts: Mutex<Value>,
    mentions: Mutex<Value>,
    /// By post id: the conversation under it, and the post itself for `GET /{id}`.
    conversations: Mutex<HashMap<String, Value>>,
    nodes: Mutex<HashMap<String, Value>>,
}

impl Mock {
    fn seen(&self, method: &str, path: &str) -> Vec<(String, String)> {
        let calls = self.calls.lock().unwrap();
        let hit = calls.iter().filter(|c| c.0 == method && c.1 == path);
        hit.map(|c| (c.2.clone(), c.3.clone())).collect()
    }
    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

fn refusal(code: i64, message: &str) -> Response {
    let body = json!({"error": {"message": message, "type": "OAuthException", "code": code}});
    (StatusCode::BAD_REQUEST, Json(body)).into_response()
}

async fn handler(State(m): State<Arc<Mock>>, method: Method, uri: Uri, body: Bytes) -> Response {
    let (path, query) = (
        uri.path().to_owned(),
        uri.query().unwrap_or_default().to_owned(),
    );
    let sent = String::from_utf8_lossy(&body).into_owned();
    m.calls
        .lock()
        .unwrap()
        .push((method.to_string(), path.clone(), query.clone(), sent));
    let mode = m.mode.load(Ordering::SeqCst);
    let publishing = path.ends_with("/threads") && method == Method::POST;
    match mode {
        1 if method == Method::GET => return refusal(190, "Error validating access token"),
        3 => return StatusCode::TOO_MANY_REQUESTS.into_response(),
        5 if publishing => return refusal(190, "Error validating access token"),
        2 if path.ends_with("/conversation") || path.ends_with("/replies") => {
            return refusal(10, "Application does not have permission for this action")
        }
        4 if path.ends_with("/mentions") => return refusal(10, "No permission"),
        6 if path.ends_with("/conversation") => return refusal(10, "Not allowed for this post"),
        _ => {}
    }
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (method.as_str(), segments.as_slice()) {
        ("GET", ["v1.0", "1", "threads"]) => Json(m.posts.lock().unwrap().clone()).into_response(),
        ("GET", ["v1.0", "1", "replies"]) => Json(json!({"data": []})).into_response(),
        ("GET", ["v1.0", "1", "mentions"]) => {
            Json(m.mentions.lock().unwrap().clone()).into_response()
        }
        ("GET", ["v1.0", id, "conversation"]) => {
            let found = m.conversations.lock().unwrap().get(*id).cloned();
            Json(found.unwrap_or(json!({"data": []}))).into_response()
        }
        ("POST", ["v1.0", "1", "threads"]) => Json(json!({"id": "C1"})).into_response(),
        ("POST", ["v1.0", "1", "threads_publish"]) => Json(json!({"id": "R1"})).into_response(),
        ("POST", ["v1.0", _, "manage_reply"]) => Json(json!({"success": true})).into_response(),
        ("GET", ["v1.0", _]) if query.contains("status") => {
            Json(json!({"status": "FINISHED"})).into_response()
        }
        ("GET", ["v1.0", id]) => {
            let found = m.nodes.lock().unwrap().get(*id).cloned();
            match found {
                Some(n) => Json(n).into_response(),
                None if *id == "R1" => {
                    Json(json!({"permalink": "https://www.threads.com/@us/post/R1"}))
                        .into_response()
                }
                None => refusal(100, "Unsupported get request"),
            }
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

struct Env {
    s: TestServer,
    db: DatabaseConnection,
    state: AppState,
    cookie: String,
    mock: Arc<Mock>,
    account: Uuid,
    team: Uuid,
}

/// A time as Threads writes it, `minutes` ago.
fn ago(minutes: i64) -> String {
    (Utc::now() - Duration::minutes(minutes))
        .format("%Y-%m-%dT%H:%M:%S+0000")
        .to_string()
}

fn reply(id: &str, user: &str, to: &str, minutes: i64) -> Value {
    json!({"id": id, "text": format!("reply {id}"), "username": user, "timestamp": ago(minutes),
        "media_type": "TEXT_POST", "is_reply": true, "is_reply_owned_by_me": user == "us",
        "has_replies": false, "replied_to": {"id": to}, "root_post": {"id": "100"},
        "permalink": format!("https://www.threads.com/@{user}/post/{id}")})
}

async fn env() -> Env {
    let mock = Arc::new(Mock::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new().fallback(handler).with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    *mock.posts.lock().unwrap() = json!({"data": [
        {"id": "100", "timestamp": ago(60 * 24), "has_replies": true},
        {"id": "101", "timestamp": ago(60 * 24), "has_replies": false}]});
    // Under our post 100: ann and a person ann answered (not for us), our own reply 201, and bob
    // under it; a mention by dee, and a mention that is also a reply (ann's).
    mock.conversations.lock().unwrap().insert(
        "100".into(),
        json!({"data": [
            reply("203", "bob", "201", 50),
            reply("202", "cat", "200", 55),
            reply("201", "us", "100", 58),
            reply("200", "ann", "100", 59)]}),
    );
    *mock.mentions.lock().unwrap() = json!({"data": [
        {"id": "300", "text": "hey @us", "username": "dee", "timestamp": ago(40),
         "media_type": "TEXT_POST", "is_reply": false, "has_replies": false,
         "permalink": "https://www.threads.com/@dee/post/300"},
        reply("200", "ann", "100", 59)]});
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    config.trust_proxy = true;
    let mut t = Threads::new(Some(("app".into(), "secret".into()))).unwrap();
    t.graph_url = base;
    t.publish_wait = Wait::from_millis(1);
    t.poll_every = Wait::from_millis(1);
    t.webhook_token = Some("hook".into());
    config.threads = t;
    let db = connect(&config).await.expect("database reachable");
    // Rows other runs left would be read by this test's ticks, through this test's mock.
    db.execute_unprepared(
        "UPDATE social_accounts SET meta = meta || '{\"reconnect\": true}' WHERE provider = 'threads'",
    )
    .await
    .unwrap();
    let state = AppState::new(db.clone(), &config);
    let s = TestServer::new(controllers::router(state.clone(), &config)).unwrap();
    let email = format!("{}@example.com", Uuid::now_v7());
    let secret = token::new();
    let b = token::new().into_bytes();
    let row = magic_links::ActiveModel {
        token_hash: Set(token::hash(&secret)),
        email: Set(email),
        expires_at: Set((Utc::now() + Duration::minutes(15)).fixed_offset()),
        ..Default::default()
    };
    magic_links::Entity::insert(row)
        .exec_without_returning(&db)
        .await
        .unwrap();
    let res = s
        .post("/auth/magic")
        .add_header("origin", "http://localhost:8080")
        .add_header("x-forwarded-for", format!("10.{}.{}.{}", b[0], b[1], b[2]))
        .form(&[("token", secret.as_str())])
        .await;
    let cookie = res
        .header("set-cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let team: Uuid = s
        .get("/v1/team")
        .add_header("cookie", cookie.clone())
        .await
        .json::<Value>()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let account = Uuid::now_v7();
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO social_accounts (id, team_id, provider, external_id, handle, access_token, \
         token_expires_at, meta, created_at) VALUES ($1, $2, 'threads', '1', 'us', $3, \
         now() + interval '40 days', '{}', now() - interval '30 days')",
        vec![
            account.into(),
            team.into(),
            state.cipher.encrypt(b"ttok").into(),
        ],
    ))
    .await
    .unwrap();
    Env {
        s,
        db,
        state,
        cookie,
        mock,
        account,
        team,
    }
}

impl Env {
    async fn sql(&self, sql: &str) {
        self.db.execute_unprepared(sql).await.unwrap();
    }
    /// Makes the account due (`full`: for the whole window too) and runs one tick.
    async fn tick(&self, full: bool) {
        let full = if full {
            "interval '2 hours'"
        } else {
            "interval '1 minute'"
        };
        self.sql(&format!(
            "UPDATE social_accounts SET threads_synced_at = now() - interval '10 minutes', \
             threads_full_at = now() - {full} WHERE id = '{}' AND threads_synced_at IS NOT NULL",
            self.account
        ))
        .await;
        threads_inbox::tick(&self.state).await.unwrap();
    }
    /// Waits for the automations a tick started in the background to end (every item of the
    /// account has `automated_at`).
    async fn settled(&self) {
        for _ in 0..100 {
            let sql = format!(
                "SELECT count(*) AS n FROM inbox_items WHERE social_account_id = '{}' \
                 AND automated_at IS NULL",
                self.account
            );
            let r = self
                .db
                .query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
                .await
                .unwrap()
                .unwrap();
            if r.try_get::<i64>("", "n").unwrap() == 0 {
                return;
            }
            tokio::time::sleep(Wait::from_millis(50)).await;
        }
        panic!("automations did not finish");
    }
    /// (kind, remote_id, parent, automated) of the account's items.
    async fn items(&self) -> Vec<(String, String, Option<String>, bool)> {
        let sql = format!(
            "SELECT kind, remote_id, parent_remote_id, automated_at IS NOT NULL AS auto \
             FROM inbox_items WHERE social_account_id = '{}' ORDER BY remote_id",
            self.account
        );
        let rows = self
            .db
            .query_all_raw(Statement::from_string(DbBackend::Postgres, sql))
            .await
            .unwrap();
        rows.iter()
            .map(|r| {
                (
                    r.try_get("", "kind").unwrap(),
                    r.try_get("", "remote_id").unwrap(),
                    r.try_get("", "parent_remote_id").unwrap(),
                    r.try_get("", "auto").unwrap(),
                )
            })
            .collect()
    }
    async fn item(&self, remote: &str) -> Uuid {
        let sql = format!(
            "SELECT id FROM inbox_items WHERE social_account_id = '{}' AND remote_id = '{remote}'",
            self.account
        );
        let r = self
            .db
            .query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
            .await
            .unwrap()
            .unwrap();
        r.try_get("", "id").unwrap()
    }
    async fn meta(&self) -> Value {
        let sql = format!(
            "SELECT meta FROM social_accounts WHERE id = '{}'",
            self.account
        );
        let r = self
            .db
            .query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
            .await
            .unwrap()
            .unwrap();
        r.try_get("", "meta").unwrap()
    }
    async fn post(&self, path: &str, body: Value) -> axum_test::TestResponse {
        self.s
            .post(path)
            .add_header("origin", WEB)
            .add_header("cookie", self.cookie.clone())
            .json(&body)
            .await
    }
}

fn kinds(items: &[(String, String, Option<String>, bool)]) -> Vec<(&str, &str)> {
    items.iter().map(|i| (i.0.as_str(), i.1.as_str())).collect()
}

#[tokio::test]
#[serial]
async fn the_first_read_stores_replies_and_mentions_as_already_automated() {
    let e = env().await;
    threads_inbox::tick(&e.state).await.unwrap();
    // Directly under our post (ann 200) or under our own reply (bob 203) are for us; cat answers
    // ann, our own reply 201 is skipped; dee's mention is one; ann's reply is not a mention twice.
    assert_eq!(
        kinds(&e.items().await),
        [("comment", "200"), ("comment", "203"), ("mention", "300")]
    );
    let items = e.items().await;
    assert!(items.iter().all(|i| i.3), "the first read answers nothing");
    assert_eq!(items[1].2.as_deref(), Some("201"));
    // The account was probed for the permission and the posts without replies were not read.
    assert_eq!(e.mock.seen("GET", "/v1.0/1/replies").len(), 1);
    assert_eq!(e.mock.seen("GET", "/v1.0/100/conversation").len(), 1);
    assert_eq!(e.mock.seen("GET", "/v1.0/101/conversation").len(), 0);
    // Nothing is due for 5 minutes.
    let calls = e.mock.count();
    threads_inbox::tick(&e.state).await.unwrap();
    assert_eq!(e.mock.count(), calls);
    // The contact is the username, and the item says what it holds.
    let sql = format!(
        "SELECT author_id, author_handle, text, permalink FROM inbox_items WHERE social_account_id = '{}' AND remote_id = '200'",
        e.account
    );
    let r =
        e.db.query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(r.try_get::<String>("", "author_id").unwrap(), "ann");
    assert_eq!(r.try_get::<String>("", "text").unwrap(), "reply 200");
    assert_eq!(
        r.try_get::<String>("", "permalink").unwrap(),
        "https://www.threads.com/@ann/post/200"
    );
}

#[tokio::test]
#[serial]
async fn a_later_pass_adds_only_what_is_new_and_leaves_old_items_unanswered() {
    let e = env().await;
    threads_inbox::tick(&e.state).await.unwrap();
    // A fresh reply, and one from 3 days ago a full pass finds late (a gap in the polling).
    {
        let mut c = e.mock.conversations.lock().unwrap();
        let all = c.get_mut("100").unwrap();
        let list = all["data"].as_array_mut().unwrap();
        list.insert(0, reply("204", "eve", "100", 1));
        list.push(reply("199", "fay", "100", 60 * 24 * 3));
    }
    e.tick(false).await;
    e.settled().await;
    // The fast pass keeps to the last 2 days of posts but takes what the conversation holds.
    let items = e.items().await;
    let fresh = items.iter().find(|i| i.1 == "204").unwrap();
    assert_eq!(
        items.iter().filter(|i| i.1 == "200").count(),
        1,
        "no duplicates"
    );
    let old = items.iter().find(|i| i.1 == "199").unwrap();
    assert!(old.3, "a reply older than a day is stored as automated");
    // A reply that came since is run through automations right away: no run row is needed to
    // see it was not left for the sweep, so it has `automated_at` once the tick ended.
    assert!(fresh.3, "automations ran over it (none matched)");
    // The fast pass makes no permission probe.
    assert_eq!(e.mock.seen("GET", "/v1.0/1/replies").len(), 1);
    e.tick(true).await;
    assert_eq!(
        e.mock.seen("GET", "/v1.0/1/replies").len(),
        2,
        "the hourly pass probes"
    );
}

#[tokio::test]
#[serial]
async fn an_automation_answers_a_new_reply_with_a_public_reply_only() {
    let e = env().await;
    threads_inbox::tick(&e.state).await.unwrap();
    let body = |trigger: &str, action: Value| {
        json!({"name": "Thanks", "trigger": trigger,
            "conditions": {"account_ids": [e.account]}, "actions": [action]})
    };
    // No DM, no follow trigger: Threads has neither.
    let res = e
        .post(
            "/v1/automations",
            body("comment_received", json!({"type": "send_dm", "text": "hi"})),
        )
        .await;
    assert_eq!(res.status_code(), 422);
    assert_eq!(res.json::<Value>()["error"], "threads_no_dm");
    let res = e
        .post(
            "/v1/automations",
            body("new_follower", json!({"type": "send_dm", "text": "hi"})),
        )
        .await;
    assert_eq!(res.json::<Value>()["error"], "threads_unsupported_trigger");
    // 126 emoji are 504 bytes: over Threads' 500.
    let long = "😀".repeat(126);
    let res = e
        .post(
            "/v1/automations",
            body(
                "comment_received",
                json!({"type": "reply_publicly", "text": long}),
            ),
        )
        .await;
    assert_eq!(res.json::<Value>()["error"], "invalid_text");
    let res = e
        .post(
            "/v1/automations",
            body(
                "comment_received",
                json!({"type": "reply_publicly", "text": "thanks!"}),
            ),
        )
        .await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    let id = res.json::<Value>()["id"].as_str().unwrap().to_owned();
    // A like does not exist on Threads; hiding a reply directly under our post does.
    let res = e
        .post(
            "/v1/automations",
            body("comment_received", json!({"type": "like_comment"})),
        )
        .await;
    assert_eq!(res.json::<Value>()["error"], "threads_no_like");
    let res = e
        .post(
            "/v1/automations",
            body("comment_received", json!({"type": "hide_comment"})),
        )
        .await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    let hide = res.json::<Value>()["id"].as_str().unwrap().to_owned();
    e.sql(&format!(
        "UPDATE automations SET activated_at = now() - interval '3 days' WHERE id IN ('{id}', '{hide}')"
    ))
    .await;
    e.mock.conversations.lock().unwrap().get_mut("100").unwrap()["data"]
        .as_array_mut()
        .unwrap()
        .insert(0, reply("205", "gus", "100", 1));
    e.tick(false).await;
    e.settled().await;
    let made = e.mock.seen("POST", "/v1.0/1/threads");
    assert_eq!(made.len(), 1, "one reply, for the new reply only");
    assert!(made[0].1.contains("reply_to_id=205") && made[0].1.contains("thanks"));
    // The second automation hid that reply (and only that one) through manage_reply.
    let hid = e.mock.seen("POST", "/v1.0/205/manage_reply");
    assert_eq!(hid.len(), 1);
    assert!(hid[0].1.contains("hide=true"));
    assert_eq!(e.mock.seen("POST", "/v1.0/1/threads_publish").len(), 1);
    // The old ones were never answered or hidden.
    e.tick(true).await;
    assert_eq!(e.mock.seen("POST", "/v1.0/1/threads").len(), 1);
    assert!(e.mock.seen("POST", "/v1.0/200/manage_reply").is_empty());
}

#[tokio::test]
#[serial]
async fn a_person_replies_from_the_inbox_within_threads_limit() {
    let e = env().await;
    threads_inbox::tick(&e.state).await.unwrap();
    let item = e.item("200").await;
    let path = format!("/v1/inbox-items/{item}/reply");
    // 501 characters, and 126 emoji (504 bytes, though only 126 characters), do not fit.
    for text in ["a".repeat(501), "😀".repeat(126)] {
        let res = e.post(&path, json!({"text": text})).await;
        assert_eq!(res.status_code(), 422);
        assert_eq!(res.json::<Value>()["error"], "invalid_text");
    }
    assert_eq!(e.mock.seen("POST", "/v1.0/1/threads").len(), 0);
    let text = format!("{}😀", "a".repeat(496));
    let res = e.post(&path, json!({"text": text})).await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    let made = e.mock.seen("POST", "/v1.0/1/threads");
    assert!(made[0].1.contains("reply_to_id=200"));
    let item_json = res.json::<Value>();
    assert_eq!(item_json["replies"][0]["remote_id"], "R1");
    // A rate limit says so, and a dead token tells the person to connect again and flags it.
    e.mock.mode.store(3, Ordering::SeqCst);
    let res = e.post(&path, json!({"text": "again"})).await;
    assert_eq!(res.status_code(), 429, "a rate limit is told as one");
    assert_eq!(res.json::<Value>()["error"], "rate_limited");
    e.mock.mode.store(5, Ordering::SeqCst);
    let other = e.item("203").await;
    let res = e
        .post(
            &format!("/v1/inbox-items/{other}/reply"),
            json!({"text": "hi"}),
        )
        .await;
    assert_eq!(res.status_code(), 422);
    let got = res.json::<Value>();
    assert_eq!(got["error"], "threads_reconnect_required");
    assert!(got["message"]
        .as_str()
        .unwrap()
        .contains("Connect the account again"));
    assert_eq!(e.meta().await["reconnect"], true);
}

#[tokio::test]
#[serial]
async fn hiding_is_confirmed_logged_and_only_for_replies_under_our_post() {
    let e = env().await;
    threads_inbox::tick(&e.state).await.unwrap();
    let (direct, nested, mention) = (
        e.item("200").await,
        e.item("203").await,
        e.item("300").await,
    );
    let path = |id: Uuid| format!("/v1/inbox-items/{id}/actions");
    let res = e.post(&path(direct), json!({"action": "hide_reply"})).await;
    assert_eq!(res.status_code(), 422);
    assert_eq!(res.json::<Value>()["error"], "confirm_required");
    assert_eq!(e.mock.seen("POST", "/v1.0/200/manage_reply").len(), 0);
    let res = e
        .post(
            &path(direct),
            json!({"action": "hide_reply", "confirm": true}),
        )
        .await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    let calls = e.mock.seen("POST", "/v1.0/200/manage_reply");
    assert!(calls[0].1.contains("hide=true"));
    let res = e
        .post(
            &path(direct),
            json!({"action": "unhide_reply", "confirm": true}),
        )
        .await;
    assert_eq!(res.status_code(), 200);
    assert!(e.mock.seen("POST", "/v1.0/200/manage_reply")[1]
        .1
        .contains("hide=false"));
    // Both are in the log, newest first, and the reply is shown again.
    let got =
        e.s.get(&path(direct))
            .add_header("cookie", e.cookie.clone())
            .await
            .json::<Value>();
    assert_eq!(got["log"][0]["action"], "unhide_reply");
    assert_eq!(got["log"][1]["action"], "hide_reply");
    assert_eq!(got["hidden"], false);
    // A reply under our own reply is nested, a mention is not under our post, and a like does
    // not exist for apps: each is refused before Threads is asked, and the refusal is logged.
    for (id, action, code) in [
        (nested, "hide_reply", "threads_nested"),
        (mention, "hide_reply", "not_our_thread"),
        (direct, "like", "threads_no_action"),
        (direct, "block_author", "threads_no_action"),
    ] {
        let res = e
            .post(&path(id), json!({"action": action, "confirm": true}))
            .await;
        assert_eq!(res.status_code(), 422, "{action}");
        assert_eq!(res.json::<Value>()["error"], code, "{action}");
    }
    assert_eq!(e.mock.seen("POST", "/v1.0/203/manage_reply").len(), 0);
}

#[tokio::test]
#[serial]
async fn a_thread_shows_the_posts_above_a_reply() {
    let e = env().await;
    threads_inbox::tick(&e.state).await.unwrap();
    {
        let mut n = e.mock.nodes.lock().unwrap();
        n.insert("201".into(), reply("201", "us", "100", 58));
        n.insert(
            "100".into(),
            json!({"id": "100", "text": "our post", "username": "us", "timestamp": ago(60 * 24),
                "is_reply": false, "media_type": "TEXT_POST"}),
        );
    }
    let item = e.item("203").await;
    let res =
        e.s.get(&format!("/v1/inbox-items/{item}/thread"))
            .add_header("cookie", e.cookie.clone())
            .await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    let got = res.json::<Value>();
    let ids: Vec<&str> = got["posts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["uri"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["100", "201"],
        "the root first, the post answered last"
    );
    assert_eq!(got["posts"][0]["text"], "our post");
    assert_eq!(got["posts"][1]["author"]["handle"], "us");
    assert_eq!(got["truncated"], false);
}

#[tokio::test]
#[serial]
async fn a_dead_token_a_missing_permission_and_a_rate_limit_each_get_their_own_handling() {
    // A dead token flags the account and pauses reading.
    let e = env().await;
    e.mock.mode.store(1, Ordering::SeqCst);
    threads_inbox::tick(&e.state).await.unwrap();
    assert_eq!(e.meta().await["reconnect"], true);
    assert!(e.items().await.is_empty());

    // An account connected before replies existed: Threads refuses the reads once, the account
    // says so, and it is left alone until it reconnects (a reconnect replaces `meta`).
    let e = env().await;
    e.mock.mode.store(2, Ordering::SeqCst);
    threads_inbox::tick(&e.state).await.unwrap();
    assert_eq!(e.meta().await["replies_denied"], true);
    let list =
        e.s.get("/v1/connections")
            .add_header("cookie", e.cookie.clone())
            .await
            .json::<Value>();
    let ours = list
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == e.account.to_string())
        .unwrap()
        .clone();
    assert_eq!(ours["replies_denied"], true);
    let calls = e.mock.count();
    e.tick(true).await;
    assert_eq!(
        e.mock.count(),
        calls,
        "an account known to lack the permission is not read"
    );
    e.sql(&format!(
        "UPDATE social_accounts SET meta = '{{}}' WHERE id = '{}'",
        e.account
    ))
    .await;
    e.mock.mode.store(0, Ordering::SeqCst);
    e.tick(true).await;
    assert!(!e.items().await.is_empty());

    // One post's conversation refused (the probe passed) is skipped, not taken as a missing permission.
    let e = env().await;
    e.mock.mode.store(6, Ordering::SeqCst);
    threads_inbox::tick(&e.state).await.unwrap();
    assert!(e.meta().await["replies_denied"].is_null());
    // Ann's reply is also in the mentions, so it comes in as one when its conversation could not be read.
    assert_eq!(
        kinds(&e.items().await),
        [("mention", "200"), ("mention", "300")]
    );

    // Missing mentions permission alone keeps the replies coming.
    let e = env().await;
    e.mock.mode.store(4, Ordering::SeqCst);
    threads_inbox::tick(&e.state).await.unwrap();
    assert_eq!(
        kinds(&e.items().await),
        [("comment", "200"), ("comment", "203")]
    );
    assert_eq!(e.meta().await["mentions_denied"], true);
    assert!(e.meta().await["replies_denied"].is_null());

    // A rate limit holds the account back for a few minutes.
    let e = env().await;
    e.mock.mode.store(3, Ordering::SeqCst);
    threads_inbox::tick(&e.state).await.unwrap();
    let sql = format!(
        "SELECT inbox_retry_at > now() AS held FROM social_accounts WHERE id = '{}'",
        e.account
    );
    let r =
        e.db.query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
            .await
            .unwrap()
            .unwrap();
    assert!(r.try_get::<bool>("", "held").unwrap());
}

#[tokio::test]
#[serial]
async fn the_webhook_hints_an_early_read_and_checks_its_signature() {
    let e = env().await;
    threads_inbox::tick(&e.state).await.unwrap();
    // The handshake.
    let res =
        e.s.get("/webhooks/threads?hub.mode=subscribe&hub.verify_token=hook&hub.challenge=42")
            .await;
    res.assert_status_ok();
    assert_eq!(res.text(), "42");
    let res =
        e.s.get("/webhooks/threads?hub.mode=subscribe&hub.verify_token=nope&hub.challenge=42")
            .await;
    assert_eq!(res.status_code(), 403);
    let payload = json!({"app_id": "app", "topic": "moderate", "target_id": "9",
        "values": [{"field": "replies", "value": {"id": "9", "root_post": {"id": "100", "owner_id": "1"}}}]});
    let body = serde_json::to_vec(&payload).unwrap();
    let due = |e: &Env| {
        let sql = format!(
            "SELECT threads_synced_at < now() - interval '4 minutes' AS due FROM social_accounts WHERE id = '{}'",
            e.account
        );
        let db = e.db.clone();
        async move {
            let r = db
                .query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
                .await
                .unwrap()
                .unwrap();
            r.try_get::<bool>("", "due").unwrap()
        }
    };
    assert!(!due(&e).await);
    // A wrong signature changes nothing.
    let res =
        e.s.post("/webhooks/threads")
            .content_type("application/json")
            .add_header("x-hub-signature-256", "sha256=00")
            .bytes(body.clone().into())
            .await;
    assert_eq!(res.status_code(), 403);
    assert!(!due(&e).await);
    // The throttle: an account read a moment ago is not asked for again.
    let sig = socialreins_core::webhook::sign_sha256("secret", &body);
    let res =
        e.s.post("/webhooks/threads")
            .content_type("application/json")
            .add_header("x-hub-signature-256", sig.clone())
            .bytes(body.clone().into())
            .await;
    assert_eq!(res.status_code(), 200);
    assert!(!due(&e).await);
    e.sql(&format!(
        "UPDATE social_accounts SET threads_synced_at = now() - interval '1 minute' WHERE id = '{}'",
        e.account
    ))
    .await;
    let res =
        e.s.post("/webhooks/threads")
            .content_type("application/json")
            .add_header("x-hub-signature-256", sig)
            .bytes(body.into())
            .await;
    assert_eq!(res.status_code(), 200);
    assert!(due(&e).await, "the delivery made the account due");
    // Nothing is stored by a delivery.
    assert_eq!(e.items().await.len(), 3);
    let _ = e.team;
}
