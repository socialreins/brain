//! The YouTube inbox against a mock YouTube (no real call): opt-in settings, the quota-aware poll
//! (cursor, budget, shared budget, quota, no retry, resuming a long sweep), what is stored and
//! shown, replies and moderation, and the 30-day purge. Needs `DATABASE_URL` like the request
//! tests.
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
    config::Config, connect, connectors::youtube::YouTube, controllers, models::magic_links,
    state::AppState, youtube_inbox,
};
use socialreins_core::token;
use uuid::Uuid;

const WEB: &str = "http://localhost:3000";
const CHANNEL: &str = "UCus";
const VIDEO: &str = "vid_12345A";

/// 0 works, 1 quota used up, 2 a server error on every read, 3 YouTube refuses the token, 4
/// comments are off on the video (every write).
#[derive(Default)]
struct Mock {
    mode: AtomicU8,
    /// (method, path, query, body)
    calls: Mutex<Vec<(String, String, String, Value)>>,
    /// Thread pages by `pageToken` ("" is the first).
    pages: Mutex<Vec<(String, Value)>>,
    replies: Mutex<Value>,
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
            let b = json!({"error": {"code": 403, "message": "quota",
                "errors": [{"reason": "quotaExceeded"}]}});
            return (StatusCode::FORBIDDEN, Json(b)).into_response();
        }
        2 if method == Method::GET => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        3 => return StatusCode::UNAUTHORIZED.into_response(),
        4 if method != Method::GET => {
            let b = json!({"error": {"code": 403, "message": "off",
                "errors": [{"reason": "commentsDisabled"}]}});
            return (StatusCode::FORBIDDEN, Json(b)).into_response();
        }
        _ => {}
    }
    match (method.as_str(), path.as_str()) {
        ("GET", "/youtube/v3/commentThreads") => {
            let token = query
                .split('&')
                .find_map(|p| p.strip_prefix("pageToken="))
                .unwrap_or_default()
                .to_owned();
            let pages = m.pages.lock().unwrap();
            let page = pages.iter().find(|(t, _)| *t == token).map(|(_, p)| p.clone());
            Json(page.unwrap_or(json!({"items": []}))).into_response()
        }
        ("GET", "/youtube/v3/comments") => Json(m.replies.lock().unwrap().clone()).into_response(),
        ("GET", "/youtube/v3/videos") => Json(json!({"items": [{"id": VIDEO, "snippet": {
            "title": "My video", "thumbnails": {"medium": {"url": "https://i.ytimg.com/vi/x/mq.jpg"}}}}]}))
        .into_response(),
        ("POST", "/youtube/v3/comments") => {
            Json(json!({"id": "UgxTHREAD1.OURREPLY", "snippet": {}})).into_response()
        }
        ("POST", "/youtube/v3/comments/setModerationStatus") => {
            StatusCode::NO_CONTENT.into_response()
        }
        ("DELETE", "/youtube/v3/comments") => StatusCode::NO_CONTENT.into_response(),
        ("POST", "/token") => {
            Json(json!({"access_token": "renewed", "expires_in": 3600})).into_response()
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
}

fn at(hours_ago: i64) -> String {
    (Utc::now() - Duration::hours(hours_ago)).to_rfc3339()
}

fn comment(id: &str, author: &str, text: &str, hours_ago: i64, parent: Option<&str>) -> Value {
    let mut c = json!({"id": id, "snippet": {"authorDisplayName": author,
        "authorChannelUrl": format!("http://www.youtube.com/@{author}"),
        "authorChannelId": {"value": format!("UC{author}")}, "textDisplay": text,
        "publishedAt": at(hours_ago)}});
    if let Some(p) = parent {
        c["snippet"]["parentId"] = json!(p);
    }
    c
}

fn thread(
    id: &str,
    author: &str,
    text: &str,
    hours_ago: i64,
    replies: Vec<Value>,
    total: u32,
) -> Value {
    json!({"id": id, "snippet": {"videoId": VIDEO, "totalReplyCount": total,
        "topLevelComment": comment(id, author, text, hours_ago, None)},
        "replies": {"comments": replies}})
}

async fn env(billing: bool) -> Env {
    let mock = Arc::new(Mock::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new().fallback(handler).with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    *mock.pages.lock().unwrap() = vec![(
        String::new(),
        json!({"items": [
            thread("UgxTHREAD2", "bob", "second comment", 1, vec![], 0),
            thread("UgxTHREAD1", "ann", "first comment", 3, vec![
                comment("UgxTHREAD1.R1", "cat", "agree with ann", 2, Some("UgxTHREAD1")),
                comment("UgxTHREAD1.R0", "us", "our own reply", 2, Some("UgxTHREAD1"))], 2)]}),
    )];
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    config.trust_proxy = true;
    config.billing_enabled = billing;
    let mut y = YouTube::new(Some(("app".into(), "secret".into()))).unwrap();
    y.api_url = base.clone();
    y.token_url = format!("{base}/token");
    config.youtube = y;
    let db = connect(&config).await.expect("database reachable");
    // Rows other runs left on would be read by this test's ticks, through this test's mock.
    db.execute_unprepared(
        "UPDATE youtube_inbox SET enabled = false, units_today = 0, write_units_today = 0; \
         DELETE FROM youtube_videos",
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
         token_expires_at, meta) VALUES ($1, $2, 'youtube', $3, 'us', $4, now() + interval '2 hours', $5)",
        vec![
            account.into(),
            team.into(),
            CHANNEL.into(),
            state.cipher.encrypt(b"ytok").into(),
            json!({"scope": "https://www.googleapis.com/auth/youtube.force-ssl"}).into(),
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
    }
}

impl Env {
    async fn put(&self, body: Value) -> axum_test::TestResponse {
        let path = format!("/v1/connections/{}/youtube-inbox", self.account);
        self.s
            .put(&path)
            .add_header("origin", WEB)
            .add_header("cookie", self.cookie.clone())
            .json(&body)
            .await
    }
    async fn get(&self) -> Value {
        let path = format!("/v1/connections/{}/youtube-inbox", self.account);
        self.s
            .get(&path)
            .add_header("cookie", self.cookie.clone())
            .await
            .json()
    }
    async fn sql(&self, sql: &str) {
        self.db.execute_unprepared(sql).await.unwrap();
    }
    /// Makes the channel due and runs one tick.
    async fn tick(&self) {
        self.sql(&format!("UPDATE youtube_inbox SET next_poll_at = now() - interval '1 minute' WHERE social_account_id = '{}'", self.account)).await;
        youtube_inbox::tick(&self.state).await.unwrap();
    }
    /// (remote_id, parent, root, automated) of the channel's items, by id.
    async fn items(&self) -> Vec<(String, Option<String>, Option<String>, bool)> {
        let rows = self
            .db
            .query_all_raw(Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT remote_id, parent_remote_id, root_remote_id, automated_at IS NOT NULL AS auto \
                         FROM inbox_items WHERE social_account_id = '{}' ORDER BY remote_id", self.account),
            ))
            .await
            .unwrap();
        rows.iter()
            .map(|r| {
                (
                    r.try_get("", "remote_id").unwrap(),
                    r.try_get("", "parent_remote_id").unwrap(),
                    r.try_get("", "root_remote_id").unwrap(),
                    r.try_get("", "auto").unwrap(),
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
    async fn cursor(&self) -> (bool, Option<String>) {
        let r = self
            .db
            .query_one_raw(Statement::from_string(
                DbBackend::Postgres,
                format!("SELECT newest_at IS NOT NULL AS has, page_token FROM youtube_inbox WHERE social_account_id = '{}'", self.account),
            ))
            .await
            .unwrap()
            .unwrap();
        (
            r.try_get("", "has").unwrap(),
            r.try_get("", "page_token").unwrap(),
        )
    }
}

#[tokio::test]
#[serial]
async fn off_by_default_and_checked_when_set() {
    let e = env(false).await;
    let got = e.get().await;
    assert_eq!(got["enabled"], false);
    assert_eq!(got["poll_minutes"], 30);
    assert_eq!(got["daily_units"], 300);
    assert_eq!(got["idle_units_per_day"], 48);
    // Nothing is read while it is off.
    youtube_inbox::tick(&e.state).await.unwrap();
    assert_eq!(e.mock.count(), 0);
    for (bad, code) in [
        (json!({"poll_minutes": 5}), "invalid_poll"),
        (json!({"poll_minutes": 2000}), "invalid_poll"),
        (json!({"daily_units": 5}), "invalid_budget"),
        (json!({"daily_units": 90000}), "invalid_budget"),
    ] {
        let res = e.put(bad).await;
        assert_eq!(res.status_code(), 422);
        assert_eq!(res.json::<Value>()["error"], code);
    }
    let res = e
        .put(json!({"enabled": true, "poll_minutes": 15, "daily_units": 200}))
        .await;
    assert_eq!(res.status_code(), 200);
    let got = res.json::<Value>();
    assert_eq!(
        (
            got["enabled"].clone(),
            got["poll_minutes"].clone(),
            got["daily_units"].clone()
        ),
        (json!(true), json!(15), json!(200))
    );
    // A channel connected without the comments permission cannot be switched on.
    e.sql(&format!(
        "UPDATE social_accounts SET meta = '{{\"scope\": \"https://www.googleapis.com/auth/youtube.upload\"}}' WHERE id = '{}'",
        e.account
    ))
    .await;
    assert_eq!(e.get().await["reconnect_for_comments"], true);
    let res = e.put(json!({"enabled": true})).await;
    assert_eq!(res.json::<Value>()["error"], "reconnect_for_comments");
    // A reconnect-flagged channel cannot be switched on either.
    let other = env(false).await;
    other
        .sql(&format!(
            "UPDATE social_accounts SET meta = meta || '{{\"reconnect\": true}}' WHERE id = '{}'",
            other.account
        ))
        .await;
    assert_eq!(other.put(json!({"enabled": true})).await.status_code(), 409);
    // Strangers see nothing.
    let stranger = env(false).await;
    let path = format!("/v1/connections/{}/youtube-inbox", e.account);
    let res = stranger
        .s
        .get(&path)
        .add_header("cookie", stranger.cookie.clone())
        .await;
    assert_eq!(res.status_code(), 404);
}

#[tokio::test]
#[serial]
async fn the_first_read_stores_history_as_automated_and_threads_replies() {
    let e = env(false).await;
    e.put(json!({"enabled": true})).await.assert_status_ok();
    e.tick().await;
    let video = Some(VIDEO.to_owned());
    // Top-level comments hang off the video, a reply off its top-level comment; our own reply
    // is not stored. The first read is history: every item is already automated.
    assert_eq!(
        e.items().await,
        vec![
            ("UgxTHREAD1".into(), video.clone(), video.clone(), true),
            (
                "UgxTHREAD1.R1".into(),
                Some("UgxTHREAD1".into()),
                video.clone(),
                true
            ),
            ("UgxTHREAD2".into(), video.clone(), video.clone(), true),
        ]
    );
    // The page, then the video's title (1 unit each).
    assert_eq!(e.mock.seen("GET", "/youtube/v3/commentThreads").len(), 1);
    assert_eq!(e.mock.seen("GET", "/youtube/v3/videos").len(), 1);
    // YouTube sent every reply, so the thread is not read again.
    assert!(e.mock.seen("GET", "/youtube/v3/comments").is_empty());
    let got = e.get().await;
    assert_eq!(got["units_today"], 2);
    assert!(got["last_read_at"].is_string());
    // The item carries the video and what a reply answers.
    let reply = e.item("UgxTHREAD1.R1").await;
    let shown =
        e.s.get(&format!("/v1/inbox-items/{reply}"))
            .add_header("cookie", e.cookie.clone())
            .await
            .json::<Value>();
    assert_eq!(shown["video"]["title"], "My video");
    assert_eq!(
        shown["video"]["thumbnail_url"],
        "https://i.ytimg.com/vi/x/mq.jpg"
    );
    assert_eq!(
        shown["video"]["url"],
        format!("https://www.youtube.com/watch?v={VIDEO}")
    );
    assert_eq!(shown["reply_to"]["text"], "first comment");
    assert_eq!(shown["kind"], "comment");
    assert_eq!(
        shown["remote_url"],
        format!("https://www.youtube.com/watch?v={VIDEO}&lc=UgxTHREAD1.R1")
    );
    // A comment that arrives later is stored (automations run on it, unlike on history);
    // nothing is stored twice.
    e.mock.pages.lock().unwrap()[0].1["items"]
        .as_array_mut()
        .unwrap()
        .insert(0, thread("UgxTHREAD3", "dan", "brand new", 0, vec![], 0));
    e.tick().await;
    let items = e.items().await;
    assert_eq!(items.len(), 4);
    assert!(items.iter().any(|i| i.0 == "UgxTHREAD3"));
    // Not due yet: no call at all.
    let calls = e.mock.count();
    youtube_inbox::tick(&e.state).await.unwrap();
    assert_eq!(e.mock.count(), calls);
}

#[tokio::test]
#[serial]
async fn a_thread_whose_replies_were_cut_or_grew_is_read_again() {
    let e = env(false).await;
    // 3 replies exist, 1 came with the thread.
    *e.mock.pages.lock().unwrap() = vec![(
        String::new(),
        json!({"items": [thread("UgxT9", "ann", "popular", 2, vec![
            comment("UgxT9.A", "bob", "one", 2, Some("UgxT9"))], 3)]}),
    )];
    *e.mock.replies.lock().unwrap() = json!({"items": [
        comment("UgxT9.A", "bob", "one", 2, Some("UgxT9")),
        comment("UgxT9.B", "cat", "two", 1, Some("UgxT9")),
        comment("UgxT9.C", "dan", "three", 1, Some("UgxT9"))]});
    e.put(json!({"enabled": true})).await.assert_status_ok();
    e.tick().await;
    let reads = e.mock.seen("GET", "/youtube/v3/comments");
    assert_eq!(reads.len(), 1);
    assert!(reads[0].0.contains("parentId=UgxT9"), "{}", reads[0].0);
    assert_eq!(e.items().await.len(), 4);
    // The count is remembered: the same thread is not read again.
    e.tick().await;
    assert_eq!(e.mock.seen("GET", "/youtube/v3/comments").len(), 1);
}

#[tokio::test]
#[serial]
async fn a_used_up_budget_stops_the_poll_until_the_pacific_day_turns() {
    let e = env(false).await;
    e.put(json!({"enabled": true, "daily_units": 20}))
        .await
        .assert_status_ok();
    e.sql(&format!(
        "UPDATE youtube_inbox SET units_today = 20 WHERE social_account_id = '{}'",
        e.account
    ))
    .await;
    e.tick().await;
    assert_eq!(e.mock.count(), 0, "no call is made without room for it");
    let got = e.get().await;
    assert_eq!(got["paused"], "budget");
    assert!(got["message"]
        .as_str()
        .unwrap()
        .contains("midnight Pacific"));
    // Raising the budget lets it go on.
    e.put(json!({"daily_units": 500})).await.assert_status_ok();
    assert_eq!(e.get().await["paused"], Value::Null);
    // A new Pacific day starts it fresh even from a pause.
    e.put(json!({"daily_units": 20})).await.assert_status_ok();
    e.sql(&format!(
        "UPDATE youtube_inbox SET paused_reason = 'budget', period = period - 1 WHERE social_account_id = '{}'",
        e.account
    ))
    .await;
    assert_eq!(e.get().await["paused"], Value::Null, "the day turned");
    e.tick().await;
    assert!(e.mock.count() > 0);
    assert_eq!(e.get().await["units_today"], 2);
}

#[tokio::test]
#[serial]
async fn all_channels_together_stop_at_the_shared_budget() {
    let e = env(false).await;
    e.put(json!({"enabled": true})).await.assert_status_ok();
    e.sql(&format!(
        "UPDATE youtube_inbox SET write_units_today = 4000 WHERE social_account_id = '{}'",
        e.account
    ))
    .await;
    e.tick().await;
    assert_eq!(e.mock.count(), 0);
    assert_eq!(e.get().await["shared_exhausted"], true);
}

#[tokio::test]
#[serial]
async fn quota_and_failures_pause_without_retrying() {
    let e = env(false).await;
    e.put(json!({"enabled": true})).await.assert_status_ok();
    e.mock.mode.store(1, Ordering::SeqCst);
    e.tick().await;
    assert_eq!(e.mock.count(), 1, "one call, never retried");
    let got = e.get().await;
    assert_eq!(got["paused"], "quota");
    assert!(got["message"].as_str().unwrap().contains("quota"));
    // Paused: not read again, however due.
    e.tick().await;
    assert_eq!(e.mock.count(), 1);
    // Five failed rounds in a row pause it.
    e.put(json!({})).await.assert_status_ok();
    e.sql(&format!(
        "UPDATE youtube_inbox SET paused_reason = NULL WHERE social_account_id = '{}'",
        e.account
    ))
    .await;
    e.mock.mode.store(2, Ordering::SeqCst);
    for _ in 0..5 {
        e.tick().await;
    }
    assert_eq!(e.get().await["paused"], "errors");
    // A token YouTube refuses flags the account and is not read again.
    let e = env(false).await;
    e.put(json!({"enabled": true})).await.assert_status_ok();
    e.mock.mode.store(3, Ordering::SeqCst);
    e.tick().await;
    assert_eq!(e.get().await["reconnect"], true);
    assert_eq!(
        e.put(json!({"enabled": true})).await.status_code(),
        200,
        "already on"
    );
}

#[tokio::test]
#[serial]
async fn a_long_sweep_goes_on_at_the_next_tick_and_loses_nothing() {
    let e = env(false).await;
    // 7 pages of one new thread each, all newer than the first read's floor.
    let mut pages = Vec::new();
    for n in 0..7 {
        let token = if n == 0 {
            String::new()
        } else {
            format!("p{n}")
        };
        let mut page =
            json!({"items": [thread(&format!("UgxP{n}"), "ann", "x", n + 1, vec![], 0)]});
        if n < 6 {
            page["nextPageToken"] = json!(format!("p{}", n + 1));
        }
        pages.push((token, page));
    }
    *e.mock.pages.lock().unwrap() = pages;
    e.put(json!({"enabled": true})).await.assert_status_ok();
    e.tick().await;
    // 5 pages a tick; the rest is left where it stopped, to go on a minute later.
    assert_eq!(e.items().await.len(), 5);
    let (has_cursor, page) = e.cursor().await;
    assert!(!has_cursor, "the cursor moves only when a sweep ends");
    assert_eq!(page.as_deref(), Some("p5"));
    // A person's Resume starts a fresh sweep: a token YouTube stopped taking cannot wedge it.
    let res =
        e.s.post(&format!(
            "/v1/connections/{}/youtube-inbox/resume",
            e.account
        ))
        .add_header("origin", WEB)
        .add_header("cookie", e.cookie.clone())
        .json(&json!({}))
        .await;
    res.assert_status_ok();
    assert_eq!(e.cursor().await.1, None);
    e.tick().await;
    assert_eq!(
        e.items().await.len(),
        5,
        "the fresh sweep read the same newest pages again"
    );
    e.tick().await;
    assert_eq!(e.items().await.len(), 7);
    let (has_cursor, page) = e.cursor().await;
    assert!(has_cursor && page.is_none());
}

#[tokio::test]
#[serial]
async fn youtube_data_past_30_days_is_deleted() {
    let e = env(false).await;
    e.put(json!({"enabled": true})).await.assert_status_ok();
    e.tick().await;
    assert_eq!(e.items().await.len(), 3);
    e.sql(&format!(
        "UPDATE inbox_items SET received_at = now() - interval '31 days' WHERE social_account_id = '{}' AND remote_id = 'UgxTHREAD1'",
        e.account
    ))
    .await;
    e.sql("UPDATE youtube_videos SET fetched_at = now() - interval '31 days'")
        .await;
    youtube_inbox::purge(&e.db).await.unwrap();
    assert_eq!(e.items().await.len(), 2);
    let left =
        e.db.query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT count(*) AS n FROM youtube_videos WHERE video_id = 'vid_12345A'".to_owned(),
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(left.try_get::<i64>("", "n").unwrap(), 0);
}

impl Env {
    async fn act(&self, item: Uuid, path: &str, body: Value) -> axum_test::TestResponse {
        self.s
            .post(&format!("/v1/inbox-items/{item}/{path}"))
            .add_header("origin", WEB)
            .add_header("cookie", self.cookie.clone())
            .json(&body)
            .await
    }
    async fn act_on(&self, path: &str, body: Value) -> axum_test::TestResponse {
        self.s
            .post(path)
            .add_header("origin", WEB)
            .add_header("cookie", self.cookie.clone())
            .json(&body)
            .await
    }
    async fn shown(&self, item: Uuid) -> Value {
        self.s
            .get(&format!("/v1/inbox-items/{item}"))
            .add_header("cookie", self.cookie.clone())
            .await
            .json()
    }
    async fn read_in(&self) {
        self.put(json!({"enabled": true})).await.assert_status_ok();
        self.tick().await;
    }
}

#[tokio::test]
#[serial]
async fn a_reply_goes_under_the_top_level_comment_and_can_be_taken_back() {
    let e = env(false).await;
    e.read_in().await;
    // A reply to a reply goes under the same top-level comment: YouTube replies are one level deep.
    let item = e.item("UgxTHREAD1.R1").await;
    let res = e.act(item, "reply", json!({"text": "thanks!"})).await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    let posted = e.mock.seen("POST", "/youtube/v3/comments");
    assert_eq!(posted.len(), 1);
    assert_eq!(posted[0].1["snippet"]["parentId"], "UgxTHREAD1");
    assert_eq!(posted[0].1["snippet"]["textOriginal"], "thanks!");
    let got = res.json::<Value>();
    assert_eq!(got["replies"][0]["remote_id"], "UgxTHREAD1.OURREPLY");
    assert!(got["replied_at"].is_string());
    assert_eq!(e.get().await["write_units_today"], 50);
    // Only the channel can delete its own reply; owners and admins do.
    let reply_id = got["replies"][0]["id"].as_str().unwrap().to_owned();
    let res = e
        .act(
            item,
            "moderate",
            json!({"action": "delete_reply", "reply_id": reply_id}),
        )
        .await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    let dels = e.mock.seen("DELETE", "/youtube/v3/comments");
    assert_eq!(dels.len(), 1);
    assert!(
        dels[0].0.contains("id=UgxTHREAD1.OURREPLY"),
        "{}",
        dels[0].0
    );
    let got = res.json::<Value>();
    assert_eq!(got["replies"].as_array().unwrap().len(), 0);
    assert_eq!(got["replied_at"], Value::Null, "unanswered again");
    assert_eq!(e.get().await["write_units_today"], 100);
    // A reply that is not this item's is not found, and nothing is called.
    let res = e
        .act(
            item,
            "moderate",
            json!({"action": "delete_reply", "reply_id": Uuid::now_v7()}),
        )
        .await;
    assert_eq!(res.status_code(), 422);
    assert_eq!(e.mock.seen("DELETE", "/youtube/v3/comments").len(), 1);
}

#[tokio::test]
#[serial]
async fn comments_can_be_held_published_rejected_and_the_author_banned() {
    let e = env(false).await;
    e.read_in().await;
    let item = e.item("UgxTHREAD2").await;
    let shown = e.shown(item).await;
    assert_eq!(shown["moderation"]["can"], json!(["hold", "reject", "ban"]));
    assert_eq!(shown["moderation"]["hidden"], false);
    assert_eq!(
        e.act(item, "moderate", json!({"action": "hold"}))
            .await
            .status_code(),
        200
    );
    let call = &e
        .mock
        .seen("POST", "/youtube/v3/comments/setModerationStatus")[0];
    assert!(
        call.0.contains("moderationStatus=heldForReview") && call.0.contains("id=UgxTHREAD2"),
        "{}",
        call.0
    );
    let shown = e.shown(item).await;
    assert_eq!(shown["moderation"]["hidden"], true);
    assert_eq!(
        shown["moderation"]["can"],
        json!(["publish", "reject", "ban"])
    );
    e.act(item, "moderate", json!({"action": "publish"}))
        .await
        .assert_status_ok();
    assert_eq!(e.shown(item).await["moderation"]["hidden"], false);
    // Rejecting is for good: the item is done and takes no reply.
    let res = e.act(item, "moderate", json!({"action": "ban"})).await;
    assert_eq!(res.status_code(), 200);
    let calls = e
        .mock
        .seen("POST", "/youtube/v3/comments/setModerationStatus");
    assert!(
        calls[2].0.contains("moderationStatus=rejected") && calls[2].0.contains("banAuthor=true"),
        "{}",
        calls[2].0
    );
    let shown = res.json::<Value>();
    assert_eq!(
        (
            shown["state"].clone(),
            shown["moderation"]["deleted"].clone()
        ),
        (json!("done"), json!(true))
    );
    assert_eq!(shown["moderation"]["can"], json!([]));
    let res = e.act(item, "reply", json!({"text": "hi"})).await;
    assert_eq!(res.status_code(), 422);
    assert_eq!(res.json::<Value>()["error"], "rejected");
    let res = e.act(item, "moderate", json!({"action": "hold"})).await;
    assert_eq!(res.json::<Value>()["error"], "not_supported");
    // Meta's words are not YouTube's.
    assert_eq!(
        e.act(item, "moderate", json!({"action": "hide"}))
            .await
            .status_code(),
        422
    );
    assert_eq!(e.get().await["write_units_today"], 150);
}

#[tokio::test]
#[serial]
async fn a_refused_write_says_what_to_do() {
    let e = env(false).await;
    e.read_in().await;
    let item = e.item("UgxTHREAD2").await;
    e.mock.mode.store(4, Ordering::SeqCst);
    let res = e.act(item, "reply", json!({"text": "hi"})).await;
    assert_eq!(res.status_code(), 422);
    let got = res.json::<Value>();
    assert_eq!(got["error"], "youtube_failed");
    assert!(
        got["message"]
            .as_str()
            .unwrap()
            .contains("comments are turned off"),
        "{got}"
    );
    let after = e.shown(item).await;
    assert_eq!(after["replies"].as_array().unwrap().len(), 0);
    assert_eq!(
        e.mock.seen("POST", "/youtube/v3/comments").len(),
        1,
        "never retried"
    );
    // A token YouTube refuses says to reconnect.
    e.mock.mode.store(3, Ordering::SeqCst);
    let res = e.act(item, "moderate", json!({"action": "hold"})).await;
    assert_eq!(res.json::<Value>()["error"], "youtube_reconnect_required");
}

#[tokio::test]
#[serial]
async fn a_channel_cannot_spend_more_than_its_write_budget_and_nothing_is_sent_for_a_rejected_thread(
) {
    let e = env(false).await;
    e.read_in().await;
    // Holding a held comment would spend 50 units for nothing: it is refused before YouTube.
    let top = e.item("UgxTHREAD1").await;
    e.act(top, "moderate", json!({"action": "hold"}))
        .await
        .assert_status_ok();
    let again = e.act(top, "moderate", json!({"action": "hold"})).await;
    assert_eq!(again.json::<Value>()["error"], "not_supported");
    assert_eq!(
        e.mock
            .seen("POST", "/youtube/v3/comments/setModerationStatus")
            .len(),
        1
    );
    // A reply under a rejected comment is hidden with it: nothing is sent.
    e.act(top, "moderate", json!({"action": "reject"}))
        .await
        .assert_status_ok();
    let reply = e.item("UgxTHREAD1.R1").await;
    let res = e.act(reply, "reply", json!({"text": "hi"})).await;
    assert_eq!(res.json::<Value>()["error"], "rejected");
    assert!(e.mock.seen("POST", "/youtube/v3/comments").is_empty());
    // Past the day's write budget (40 calls) every write is refused with the reason.
    e.sql(&format!(
        "UPDATE youtube_inbox SET write_units_today = 1980 WHERE social_account_id = '{}'",
        e.account
    ))
    .await;
    let other = e.item("UgxTHREAD2").await;
    let res = e.act(other, "reply", json!({"text": "hi"})).await;
    assert_eq!(res.status_code(), 422);
    let got = res.json::<Value>();
    assert_eq!(got["error"], "youtube_write_budget");
    assert!(got["message"]
        .as_str()
        .unwrap()
        .contains("midnight Pacific"));
    assert_eq!(
        e.act(other, "moderate", json!({"action": "reject"}))
            .await
            .status_code(),
        422
    );
}

#[tokio::test]
#[serial]
async fn automations_on_youtube_are_comment_triggers_with_public_replies_only() {
    let e = env(false).await;
    let make = |trigger: &str, actions: Value| {
        json!({"name": "Thanks", "trigger": trigger,
            "conditions": {"account_ids": [e.account]}, "actions": actions})
    };
    let post = |body: Value| e.act_on("/v1/automations", body);
    // Only comments exist on YouTube, and only a public reply can answer one.
    let res = post(make(
        "dm_received",
        json!([{"type": "send_dm", "text": "hi"}]),
    ))
    .await;
    assert_eq!(res.json::<Value>()["error"], "youtube_unsupported_trigger");
    let res = post(make(
        "comment_received",
        json!([{"type": "send_dm", "text": "hi"}]),
    ))
    .await;
    assert_eq!(res.json::<Value>()["error"], "youtube_no_dm");
    let res = post(make(
        "comment_received",
        json!([{"type": "reply_publicly", "text": "Thanks {name}!"}]),
    ))
    .await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    // "Test this automation" matches a sample comment and shows the reply it would send.
    let test = json!({"automation": make("comment_received", json!([{"type": "reply_publicly", "text": "Thanks {name}!"}])),
        "sample": {"account_id": e.account, "text": "love it", "author_name": "Ann"}});
    let res = e.act_on("/v1/automations/test", test).await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    let out = res.json::<Value>();
    assert_eq!(out["matched"], true);
    assert_eq!(out["blocks"], json!([]));
    assert_eq!(out["steps"][0]["status"], "would_send");
    assert_eq!(out["steps"][0]["text"], "Thanks Ann!");
    // A DM in the test is refused with the reason it could never go (the same check as saving).
    let test = json!({"automation": make("comment_received", json!([{"type": "send_dm", "text": "hi"}])),
        "sample": {"account_id": e.account, "text": "love it"}});
    let res = e.act_on("/v1/automations/test", test).await;
    assert_eq!(res.status_code(), 422);
    assert_eq!(res.json::<Value>()["error"], "youtube_no_dm");
    // A new comment is answered once, in public, under its top-level comment.
    e.read_in().await;
    e.mock.pages.lock().unwrap()[0].1["items"]
        .as_array_mut()
        .unwrap()
        .insert(0, thread("UgxTHREAD9", "dan", "brand new", 0, vec![], 0));
    e.tick().await;
    let sent = e.mock.seen("POST", "/youtube/v3/comments");
    assert_eq!(
        sent.len(),
        1,
        "one reply, for the comment that arrived after the first read"
    );
    assert_eq!(sent[0].1["snippet"]["parentId"], "UgxTHREAD9");
    assert_eq!(sent[0].1["snippet"]["textOriginal"], "Thanks dan!");
}

#[tokio::test]
#[serial]
async fn a_dm_step_of_an_unscoped_automation_is_skipped_on_youtube_and_nothing_is_sent() {
    let e = env(false).await;
    // No account list: it is saved, and a YouTube comment meets a DM it can never send.
    let body = json!({"name": "Welcome", "trigger": "comment_received",
        "actions": [{"type": "send_dm", "text": "hi"}]});
    let res = e.act_on("/v1/automations", body).await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    let id = res.json::<Value>()["id"].as_str().unwrap().to_owned();
    e.read_in().await;
    e.mock.pages.lock().unwrap()[0].1["items"]
        .as_array_mut()
        .unwrap()
        .insert(0, thread("UgxTHREAD8", "eve", "fresh", 0, vec![], 0));
    e.tick().await;
    assert!(e.mock.seen("POST", "/youtube/v3/comments").is_empty());
    let runs =
        e.s.get(&format!("/v1/automations/{id}/runs"))
            .add_header("cookie", e.cookie.clone())
            .await
            .json::<Value>();
    let run = &runs["runs"][0];
    assert_eq!(
        (run["status"].clone(), run["reason"].clone()),
        (json!("skipped"), json!("youtube_no_dm")),
        "{runs}"
    );
}
