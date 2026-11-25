//! The X inbox against a mock X (no real X call): opt-in settings, the cost-aware poll (cursors,
//! budget, credits, no retry), what is stored, replies, DMs, actions and team credits. Needs
//! `DATABASE_URL` like the request tests.
use std::sync::{
    atomic::{AtomicU8, Ordering},
    Arc, Mutex,
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
    config::Config, connect, connectors::x::X, controllers, models::magic_links, state::AppState,
    x_inbox,
};
use socialreins_core::token;
use uuid::Uuid;

const WEB: &str = "http://localhost:3000";
const ALL: &str = "tweet.read tweet.write users.read offline.access media.write dm.read dm.write like.write tweet.moderate.write";
const OLD: &str = "tweet.read tweet.write users.read offline.access media.write";

/// 0 works, 1 out of credits, 2 usage cap, 3 refuses DMs and posts (not allowed), 4 a failure.
#[derive(Default)]
struct Mock {
    mode: AtomicU8,
    /// (method, path, query, body)
    calls: Mutex<Vec<(String, String, String, Value)>>,
    mentions: Mutex<Value>,
    dms: Mutex<Value>,
}

impl Mock {
    fn seen(&self, method: &str, path: &str) -> Vec<(String, Value)> {
        let calls = self.calls.lock().unwrap();
        let hit = calls.iter().filter(|c| c.0 == method && c.1 == path);
        hit.map(|c| (c.2.clone(), c.3.clone())).collect()
    }
    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

async fn handler(State(m): State<Arc<Mock>>, method: Method, uri: Uri, body: Bytes) -> Response {
    let (path, query) = (
        uri.path().to_owned(),
        uri.query().unwrap_or_default().to_owned(),
    );
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    m.calls
        .lock()
        .unwrap()
        .push((method.to_string(), path.clone(), query.clone(), parsed));
    match m.mode.load(Ordering::SeqCst) {
        1 => {
            let b = json!({"title": "CreditsDepleted", "detail": "Your app has no credits."});
            return (StatusCode::PAYMENT_REQUIRED, Json(b)).into_response();
        }
        2 => {
            let b = json!({"type": "https://api.x.com/2/problems/usage-capped"});
            return (StatusCode::TOO_MANY_REQUESTS, Json(b)).into_response();
        }
        3 if path.contains("messages") => {
            let b = json!({"type": "https://api.x.com/2/problems/dm-recipient-not-accepting",
                "detail": "This person does not accept your messages."});
            return (StatusCode::FORBIDDEN, Json(b)).into_response();
        }
        3 | 4 if method != Method::GET => {
            let b = json!({"detail": "Nope."});
            return (StatusCode::FORBIDDEN, Json(b)).into_response();
        }
        _ => {}
    }
    match (method.as_str(), path.as_str()) {
        ("GET", "/2/users/1/mentions") if query.contains("since_id") => {
            Json(json!({"meta": {"result_count": 0}})).into_response()
        }
        ("GET", "/2/users/1/mentions") => Json(m.mentions.lock().unwrap().clone()).into_response(),
        ("GET", "/2/dm_events") => {
            let all = m.dms.lock().unwrap().clone();
            if query.contains("max_results=1&") || query.starts_with("max_results=1&") {
                let first = all["data"].as_array().and_then(|d| d.first().cloned());
                let mut one = all.clone();
                one["data"] = json!(first.into_iter().collect::<Vec<_>>());
                return Json(one).into_response();
            }
            Json(all).into_response()
        }
        ("POST", p) if p.ends_with("/messages") => (
            StatusCode::CREATED,
            Json(json!({"data": {"dm_conversation_id": "7-1", "dm_event_id": "900"}})),
        )
            .into_response(),
        ("POST", "/2/tweets") => (
            StatusCode::CREATED,
            Json(json!({"data": {"id": "901", "text": "x"}})),
        )
            .into_response(),
        ("POST", p) if p.ends_with("/likes") => {
            Json(json!({"data": {"liked": true}})).into_response()
        }
        ("POST", p) if p.ends_with("/retweets") => {
            Json(json!({"data": {"retweeted": true}})).into_response()
        }
        ("PUT", p) if p.ends_with("/hidden") => {
            Json(json!({"data": {"hidden": true}})).into_response()
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

fn tweet(id: &str, author: &str, text: &str, reply_to: Option<(&str, &str)>) -> Value {
    let mut t = json!({"id": id, "text": text, "author_id": author,
        "created_at": "2026-11-24T10:00:00.000Z", "conversation_id": "100"});
    if let Some((tweet, user)) = reply_to {
        t["in_reply_to_user_id"] = json!(user);
        t["referenced_tweets"] = json!([{"type": "replied_to", "id": tweet}]);
    }
    t
}

fn users() -> Value {
    json!([{"id": "7", "username": "ann", "name": "Ann"}, {"id": "8", "username": "bob", "name": "Bob"},
           {"id": "1", "username": "us", "name": "Us"}])
}

async fn env(scope: &str, billing: bool) -> Env {
    let mock = Arc::new(Mock::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new().fallback(handler).with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    *mock.mentions.lock().unwrap() = json!({
        "data": [
            tweet("30", "7", "@us love this", Some(("20", "1"))),
            tweet("29", "8", "hello @us", None),
            tweet("28", "1", "@us our own reply", Some(("20", "1")))],
        "includes": {"users": users()},
        "meta": {"newest_id": "30", "oldest_id": "28", "result_count": 3}});
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    config.trust_proxy = true;
    config.billing_enabled = billing;
    let mut x = X::new(None).unwrap();
    x.api_url = base;
    config.x = x;
    let db = connect(&config).await.expect("database reachable");
    // Rows other runs left on would be read by this test's ticks, through this test's mock.
    db.execute_unprepared("UPDATE x_inbox SET enabled = false")
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
         token_expires_at, meta) VALUES ($1, $2, 'x', '1', 'us', $3, now() + interval '2 hours', $4)",
        vec![
            account.into(),
            team.into(),
            state.cipher.encrypt(b"xtok").into(),
            json!({"scope": scope}).into(),
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
    async fn put(&self, body: Value) -> axum_test::TestResponse {
        let path = format!("/v1/connections/{}/x-inbox", self.account);
        self.s
            .put(&path)
            .add_header("origin", WEB)
            .add_header("cookie", self.cookie.clone())
            .json(&body)
            .await
    }
    async fn get(&self) -> Value {
        let path = format!("/v1/connections/{}/x-inbox", self.account);
        self.s
            .get(&path)
            .add_header("cookie", self.cookie.clone())
            .await
            .json()
    }
    async fn sql(&self, sql: &str) {
        self.db.execute_unprepared(sql).await.unwrap();
    }
    /// Makes the account due and runs one tick.
    async fn tick(&self) {
        self.sql(&format!("UPDATE x_inbox SET next_poll_at = now() - interval '1 minute' WHERE social_account_id = '{}'", self.account)).await;
        x_inbox::tick(&self.state).await.unwrap();
    }
    async fn items(&self) -> Vec<(String, String, Option<String>)> {
        let rows = self
            .db
            .query_all_raw(Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT kind, remote_id, parent_remote_id FROM inbox_items WHERE social_account_id = '{}' ORDER BY remote_id", self.account),
            ))
            .await
            .unwrap();
        rows.iter()
            .map(|r| {
                (
                    r.try_get("", "kind").unwrap(),
                    r.try_get("", "remote_id").unwrap(),
                    r.try_get("", "parent_remote_id").unwrap(),
                )
            })
            .collect()
    }
    async fn item(&self, remote: &str) -> Uuid {
        let r = self
            .db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT id FROM inbox_items WHERE social_account_id = '{}' AND remote_id = '{remote}'", self.account),
            ))
            .await
            .unwrap()
            .unwrap();
        r.try_get("", "id").unwrap()
    }
    async fn post(&self, path: &str, body: Value) -> axum_test::TestResponse {
        self.s
            .post(path)
            .add_header("origin", WEB)
            .add_header("cookie", self.cookie.clone())
            .json(&body)
            .await
    }
    async fn credits(&self) -> i64 {
        socialreins_brain::billing::balance(
            &self.db,
            self.team,
            socialreins_brain::billing::Kind::X,
        )
        .await
        .unwrap()
    }
}

#[tokio::test]
#[serial]
async fn off_by_default_and_checked_when_set() {
    let e = env(OLD, false).await;
    let got = e.get().await;
    assert_eq!(got["enabled"], false);
    assert_eq!(got["poll_minutes"], 60);
    assert_eq!(got["budget_usd"], 2.0);
    assert_eq!(
        got["can_read_dms"], false,
        "an account connected before the inbox scopes"
    );
    // Nothing is read while it is off.
    x_inbox::tick(&e.state).await.unwrap();
    assert_eq!(e.mock.count(), 0);
    for (bad, code) in [
        (json!({"poll_minutes": 5}), "invalid_poll"),
        (json!({"poll_minutes": 2000}), "invalid_poll"),
        (json!({"budget_usd": 0.01}), "invalid_budget"),
        (json!({"budget_usd": 5000}), "invalid_budget"),
        (json!({"read_dms": true}), "reconnect_for_dms"),
    ] {
        let res = e.put(bad).await;
        assert_eq!(res.status_code(), 422);
        assert_eq!(res.json::<Value>()["error"], code);
    }
    let res = e
        .put(json!({"enabled": true, "poll_minutes": 30, "budget_usd": 1.5}))
        .await;
    assert_eq!(res.status_code(), 200);
    let got = res.json::<Value>();
    assert_eq!(
        (
            got["enabled"].clone(),
            got["poll_minutes"].clone(),
            got["budget_usd"].clone()
        ),
        (json!(true), json!(30), json!(1.5))
    );
    // The estimate is shown before anything is spent.
    assert_eq!(got["estimate"]["per_mention_usd"], 0.015);
    assert_eq!(got["spent_usd"], 0.0);
    // A reconnect-flagged account cannot be switched on.
    let other = env(ALL, false).await;
    other
        .sql(&format!(
            "UPDATE social_accounts SET meta = meta || '{{\"reconnect\": true}}' WHERE id = '{}'",
            other.account
        ))
        .await;
    assert_eq!(other.put(json!({"enabled": true})).await.status_code(), 409);
    // Strangers see nothing.
    let stranger = env(ALL, false).await;
    let path = format!("/v1/connections/{}/x-inbox", e.account);
    let res = stranger
        .s
        .get(&path)
        .add_header("cookie", stranger.cookie.clone())
        .await;
    assert_eq!(res.status_code(), 404);
}

#[tokio::test]
#[serial]
async fn the_poll_stores_what_is_new_counts_its_cost_and_uses_the_cursor() {
    let e = env(ALL, false).await;
    e.put(json!({"enabled": true})).await.assert_status_ok();
    e.tick().await;
    // A reply to our post is a comment, any other post that mentions us a mention; our own is skipped.
    assert_eq!(
        e.items().await,
        vec![
            ("mention".to_owned(), "29".to_owned(), None),
            ("comment".to_owned(), "30".to_owned(), Some("20".to_owned())),
        ]
    );
    let first = e.mock.seen("GET", "/2/users/1/mentions");
    assert_eq!(first.len(), 1);
    assert!(
        first[0].0.contains("start_time=") && !first[0].0.contains("since_id"),
        "{}",
        first[0].0
    );
    // 3 posts and 3 users: 3 * 5000 + 3 * 10000 millionths of a dollar.
    let got = e.get().await;
    assert_eq!(got["spent_usd"], 0.045);
    assert!(got["last_read_at"].is_string());
    // The next poll asks only for what is after the newest post, and costs nothing when none is.
    e.tick().await;
    let second = e.mock.seen("GET", "/2/users/1/mentions");
    assert_eq!(second.len(), 2);
    assert!(second[1].0.contains("since_id=30"), "{}", second[1].0);
    assert_eq!(e.get().await["spent_usd"], 0.045);
    assert_eq!(e.items().await.len(), 2, "nothing stored twice");
    // Not due yet: no call at all.
    let calls = e.mock.count();
    x_inbox::tick(&e.state).await.unwrap();
    assert_eq!(e.mock.count(), calls);
}

#[tokio::test]
#[serial]
async fn a_used_up_budget_stops_the_poll() {
    let e = env(ALL, false).await;
    e.put(json!({"enabled": true, "budget_usd": 0.1}))
        .await
        .assert_status_ok();
    e.sql(&format!(
        "UPDATE x_inbox SET spent_micros = 90000 WHERE social_account_id = '{}'",
        e.account
    ))
    .await;
    e.tick().await;
    assert_eq!(e.mock.count(), 0, "no call is made without room for it");
    let got = e.get().await;
    assert_eq!(got["paused"], "budget");
    assert!(got["message"].as_str().unwrap().contains("budget"));
    // Raising the budget lets it go on; a new month would too.
    e.put(json!({"budget_usd": 1})).await.assert_status_ok();
    assert_eq!(e.get().await["paused"], Value::Null);
    e.tick().await;
    assert_eq!(e.mock.seen("GET", "/2/users/1/mentions").len(), 1);
}

#[tokio::test]
#[serial]
async fn no_credits_stops_the_poll_and_nothing_retries_it() {
    let e = env(ALL, false).await;
    e.put(json!({"enabled": true})).await.assert_status_ok();
    e.mock.mode.store(1, Ordering::SeqCst);
    e.tick().await;
    assert_eq!(e.mock.count(), 1);
    let got = e.get().await;
    assert_eq!(got["paused"], "credits");
    assert!(
        got["message"]
            .as_str()
            .unwrap()
            .contains("out of API credits"),
        "{got}"
    );
    // However many rounds pass, X is not asked again until a person resumes.
    for _ in 0..3 {
        e.tick().await;
    }
    assert_eq!(e.mock.count(), 1);
    e.mock.mode.store(0, Ordering::SeqCst);
    let res = e
        .post(
            &format!("/v1/connections/{}/x-inbox/resume", e.account),
            json!({}),
        )
        .await;
    assert_eq!(res.status_code(), 200);
    assert_eq!(res.json::<Value>()["paused"], Value::Null);
    e.tick().await;
    assert_eq!(e.mock.count(), 2);
    assert_eq!(e.items().await.len(), 2);
    // The usage cap pauses the same way.
    e.mock.mode.store(2, Ordering::SeqCst);
    e.sql(&format!(
        "UPDATE x_inbox SET mentions_since_id = NULL WHERE social_account_id = '{}'",
        e.account
    ))
    .await;
    e.tick().await;
    assert_eq!(e.get().await["paused"], "usage_capped");
}

#[tokio::test]
#[serial]
async fn other_failures_wait_for_the_interval_and_five_in_a_row_pause() {
    let e = env(ALL, false).await;
    e.put(json!({"enabled": true})).await.assert_status_ok();
    e.mock.mode.store(4, Ordering::SeqCst);
    // A failing GET: the mock only fails writes in mode 4, so use the missing route instead.
    *e.mock.mentions.lock().unwrap() = Value::Null;
    e.sql(&format!(
        "UPDATE social_accounts SET external_id = '55' WHERE id = '{}'",
        e.account
    ))
    .await;
    for _ in 0..5 {
        e.tick().await;
    }
    let got = e.get().await;
    assert_eq!(got["paused"], "errors", "{got}");
    assert_eq!(e.mock.count(), 5, "one call a round, never more");
}

#[tokio::test]
#[serial]
async fn dms_are_read_with_a_probe_and_only_new_ones_are_paged() {
    let e = env(ALL, false).await;
    *e.mock.dms.lock().unwrap() = json!({
        "data": [
            {"id": "502", "event_type": "MessageCreate", "text": "hi there", "sender_id": "7",
             "dm_conversation_id": "7-1", "created_at": "2026-11-24T10:05:00.000Z"},
            {"id": "501", "event_type": "MessageCreate", "text": "our reply", "sender_id": "1",
             "dm_conversation_id": "7-1", "created_at": "2026-11-24T10:04:00.000Z"}],
        "includes": {"users": users()}, "meta": {"result_count": 2}});
    e.put(json!({"enabled": true, "read_dms": true}))
        .await
        .assert_status_ok();
    e.tick().await;
    let items = e.items().await;
    assert!(
        items.contains(&("dm".to_owned(), "502".to_owned(), None)),
        "{items:?}"
    );
    assert!(
        !items.iter().any(|i| i.1 == "501"),
        "our own message is not an item"
    );
    // A probe of one, then a page.
    assert_eq!(e.mock.seen("GET", "/2/dm_events").len(), 2);
    // Next round: the probe shows nothing new, so no page is read.
    e.tick().await;
    assert_eq!(e.mock.seen("GET", "/2/dm_events").len(), 3);
    let got = e.get().await;
    assert_eq!(got["can_read_dms"], true);
}

#[tokio::test]
#[serial]
async fn replies_dms_and_actions_go_out_once_and_say_why_when_refused() {
    let e = env(ALL, false).await;
    e.put(json!({"enabled": true})).await.assert_status_ok();
    e.tick().await;
    let comment = e.item("30").await;
    let res = e
        .post(
            &format!("/v1/inbox-items/{comment}/reply"),
            json!({"text": "thanks!"}),
        )
        .await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    let sent = e.mock.seen("POST", "/2/tweets");
    assert_eq!(sent[0].1["reply"]["in_reply_to_tweet_id"], "30");
    assert_eq!(sent[0].1["text"], "thanks!");
    // Too long for a reply.
    let long = "a".repeat(300);
    let res = e
        .post(
            &format!("/v1/inbox-items/{comment}/reply"),
            json!({"text": long}),
        )
        .await;
    assert_eq!(res.status_code(), 422);
    // Like, repost, hide.
    for (action, method, path) in [
        ("like", "POST", "/2/users/1/likes"),
        ("repost", "POST", "/2/users/1/retweets"),
        ("hide", "PUT", "/2/tweets/30/hidden"),
    ] {
        let res = e
            .post(
                &format!("/v1/inbox-items/{comment}/x-action"),
                json!({"action": action}),
            )
            .await;
        assert_eq!(res.status_code(), 204, "{action}");
        assert_eq!(e.mock.seen(method, path).len(), 1, "{action}");
    }
    // Hiding is for replies to our posts, not mentions.
    let mention = e.item("29").await;
    let res = e
        .post(
            &format!("/v1/inbox-items/{mention}/x-action"),
            json!({"action": "hide"}),
        )
        .await;
    assert_eq!(res.status_code(), 422);
    assert_eq!(res.json::<Value>()["error"], "not_repliable");
    // A DM goes to the conversation; X refusing a closed inbox is `dm_not_allowed`.
    e.sql(&format!("INSERT INTO inbox_items (id, team_id, social_account_id, platform, kind, remote_id, convo_id, author_id, text, received_at, state) VALUES (gen_random_uuid(), '{}', '{}', 'x', 'dm', '777', '7-1', '7', 'hello', now(), 'open')", e.team, e.account)).await;
    let dm = e.item("777").await;
    let res = e
        .post(
            &format!("/v1/inbox-items/{dm}/reply"),
            json!({"text": "hi"}),
        )
        .await;
    assert_eq!(res.status_code(), 201);
    assert_eq!(
        e.mock
            .seen("POST", "/2/dm_conversations/7-1/messages")
            .len(),
        1
    );
    e.mock.mode.store(3, Ordering::SeqCst);
    let res = e
        .post(
            &format!("/v1/inbox-items/{dm}/reply"),
            json!({"text": "hi"}),
        )
        .await;
    assert_eq!(res.status_code(), 422);
    assert_eq!(res.json::<Value>()["error"], "dm_not_allowed");
}

#[tokio::test]
#[serial]
async fn an_account_without_the_scopes_is_told_to_reconnect() {
    let e = env(OLD, false).await;
    e.put(json!({"enabled": true})).await.assert_status_ok();
    e.tick().await;
    let comment = e.item("30").await;
    // Mentions need no new scope.
    assert_eq!(e.items().await.len(), 2);
    let res = e
        .post(
            &format!("/v1/inbox-items/{comment}/x-action"),
            json!({"action": "like"}),
        )
        .await;
    assert_eq!(res.status_code(), 422);
    assert_eq!(res.json::<Value>()["error"], "x_reconnect_required");
    e.sql(&format!("INSERT INTO inbox_items (id, team_id, social_account_id, platform, kind, remote_id, convo_id, author_id, text, received_at, state) VALUES (gen_random_uuid(), '{}', '{}', 'x', 'dm', '778', '7-1', '7', 'hello', now(), 'open')", e.team, e.account)).await;
    let dm = e.item("778").await;
    let res = e
        .post(
            &format!("/v1/inbox-items/{dm}/reply"),
            json!({"text": "hi"}),
        )
        .await;
    assert_eq!(res.json::<Value>()["error"], "x_reconnect_required");
    assert_eq!(
        e.mock
            .seen("POST", "/2/dm_conversations/7-1/messages")
            .len(),
        0
    );
}

#[tokio::test]
#[serial]
async fn with_billing_replies_and_reads_spend_team_credits() {
    let e = env(ALL, true).await;
    socialreins_brain::billing::grant(
        &e.db,
        e.team,
        socialreins_brain::billing::Kind::X,
        2,
        "test",
        Some("g"),
    )
    .await
    .unwrap();
    e.put(json!({"enabled": true})).await.assert_status_ok();
    // Billing on without a live plan: nothing is read.
    e.tick().await;
    assert_eq!(e.mock.count(), 0);
    e.sql(&format!("INSERT INTO inbox_items (id, team_id, social_account_id, platform, kind, remote_id, parent_remote_id, author_id, text, received_at, state) VALUES (gen_random_uuid(), '{}', '{}', 'x', 'comment', '31', '20', '7', 'hey', now(), 'open')", e.team, e.account)).await;
    let item = e.item("31").await;
    let path = format!("/v1/inbox-items/{item}/reply");
    assert_eq!(
        e.post(&path, json!({"text": "one"})).await.status_code(),
        201
    );
    assert_eq!(e.credits().await, 1);
    // A refusal gives the credit back.
    e.mock.mode.store(3, Ordering::SeqCst);
    assert_eq!(
        e.post(&path, json!({"text": "two"})).await.status_code(),
        422
    );
    assert_eq!(e.credits().await, 1);
    e.mock.mode.store(0, Ordering::SeqCst);
    assert_eq!(
        e.post(&path, json!({"text": "three"})).await.status_code(),
        201
    );
    assert_eq!(e.credits().await, 0);
    // None left: refused before X is asked.
    let calls = e.mock.count();
    let res = e.post(&path, json!({"text": "four"})).await;
    assert_eq!(res.status_code(), 422);
    assert!(res.json::<Value>()["message"]
        .as_str()
        .unwrap()
        .contains("credit"));
    assert_eq!(e.mock.count(), calls);
}
