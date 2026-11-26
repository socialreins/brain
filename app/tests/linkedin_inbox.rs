//! LinkedIn comments and automations against a mock LinkedIn (no real call). The capability is
//! off until LinkedIn approves the Community Management API and a Page holds its scopes: these
//! tests cover the states, the refusals, the poll, a reply and a like. Needs `DATABASE_URL` like
//! the request tests.
use std::sync::{Arc, Mutex};

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json, Router,
};
use axum_test::TestServer;
use chrono::{Duration, Utc};
use sea_orm::{
    ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait,
    QueryFilter, Statement,
};
use serde_json::{json, Value};
use serial_test::serial;
use socialreins_brain::{
    config::Config,
    connect,
    connectors::{bluesky::InboxError, linkedin::LinkedIn},
    controllers, linkedin_inbox,
    models::{inbox_items, magic_links, social_accounts},
    state::AppState,
};
use socialreins_core::token;
use uuid::Uuid;

const WEB: &str = "http://localhost:3000";
const ORG: &str = "urn:li:organization:99";
const POST: &str = "urn:li:share:7001";

#[derive(Default)]
struct Mock {
    /// (method, path, query, body)
    calls: Mutex<Vec<(String, String, String, Value)>>,
    comments: Mutex<Vec<Value>>,
}

impl Mock {
    fn seen(&self, method: &str, prefix: &str) -> Vec<(String, Value)> {
        let calls = self.calls.lock().unwrap();
        let hit = calls
            .iter()
            .filter(|c| c.0 == method && c.1.starts_with(prefix));
        hit.map(|c| (c.1.clone() + "?" + &c.2, c.3.clone()))
            .collect()
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
        .push((method.to_string(), path.clone(), query, parsed));
    match (method.as_str(), path.as_str()) {
        ("GET", p) if p.ends_with("/comments") => {
            Json(json!({"elements": m.comments.lock().unwrap().clone()})).into_response()
        }
        ("POST", p) if p.ends_with("/comments") => {
            let mut h = HeaderMap::new();
            h.insert(
                "x-restli-id",
                HeaderValue::from_static("urn:li:comment:(urn:li:activity:7001,555)"),
            );
            (StatusCode::CREATED, h).into_response()
        }
        ("POST", "/rest/reactions") => StatusCode::CREATED.into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

fn ago(minutes: i64) -> i64 {
    (Utc::now() - Duration::minutes(minutes)).timestamp_millis()
}

fn comment(n: u32, actor: &str, text: &str, minutes: i64) -> Value {
    json!({"commentUrn": format!("urn:li:comment:(urn:li:activity:7001,{n})"),
        "actor": actor, "message": {"text": text}, "created": {"time": ago(minutes)}})
}

struct Env {
    s: TestServer,
    db: DatabaseConnection,
    state: AppState,
    cookie: String,
    mock: Arc<Mock>,
    account: Uuid,
}

async fn env(approved: bool, meta: Value) -> Env {
    let mock = Arc::new(Mock::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new().fallback(handler).with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    config.trust_proxy = true;
    let mut l = LinkedIn::new(Some(("app".into(), "secret".into()))).unwrap();
    l.api_url = base;
    l.community_api = approved;
    config.linkedin = l;
    let db = connect(&config).await.expect("database reachable");
    // Pages other runs left would be read by this test's ticks, through this test's mock.
    db.execute_unprepared(
        "UPDATE social_accounts SET meta = meta - 'organization' WHERE provider = 'linkedin'",
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
        "INSERT INTO social_accounts (id, team_id, provider, external_id, access_token, \
         token_expires_at, meta) VALUES ($1, $2, 'linkedin', '99', $3, now() + interval '30 days', $4)",
        vec![account.into(), team.into(), state.cipher.encrypt(b"litok").into(), meta.into()],
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

fn page() -> Value {
    json!({"organization": ORG,
        "scopes": ["r_organization_social_feed", "w_organization_social_feed"]})
}

impl Env {
    async fn sql(&self, sql: &str) {
        self.db.execute_unprepared(sql).await.unwrap();
    }
    async fn get(&self, path: &str) -> Value {
        self.s
            .get(path)
            .add_header("cookie", self.cookie.clone())
            .await
            .json()
    }
    async fn act_on(&self, path: &str, body: Value) -> axum_test::TestResponse {
        self.s
            .post(path)
            .add_header("origin", WEB)
            .add_header("cookie", self.cookie.clone())
            .json(&body)
            .await
    }
    /// A published post of the Page, whose comments are read.
    async fn published(&self) {
        let (post, set) = (Uuid::now_v7(), Uuid::now_v7());
        self.sql(&format!(
            "INSERT INTO social_sets (id, team_id, name) SELECT '{set}', team_id, 'li' \
               FROM social_accounts WHERE id = '{a}'; \
             INSERT INTO posts (id, team_id, social_set_id, platform, status) \
               SELECT '{post}', team_id, '{set}', 'linkedin', 'published' FROM social_accounts WHERE id = '{a}'; \
             INSERT INTO post_targets (id, post_id, social_account_id, status, remote_id, published_at) \
               VALUES (gen_random_uuid(), '{post}', '{a}', 'published', '{POST}', now())",
            a = self.account
        ))
        .await;
    }
    async fn tick(&self) {
        self.sql(&format!(
            "UPDATE social_accounts SET meta = meta #- '{{linkedin_inbox,next_at}}' WHERE id = '{}'",
            self.account
        ))
        .await;
        linkedin_inbox::tick(&self.state).await.unwrap();
    }
    async fn items(&self) -> Vec<inbox_items::Model> {
        inbox_items::Entity::find()
            .filter(inbox_items::Column::SocialAccountId.eq(self.account))
            .all(&self.db)
            .await
            .unwrap()
    }
    async fn model(&self) -> social_accounts::Model {
        social_accounts::Entity::find_by_id(self.account)
            .one(&self.db)
            .await
            .unwrap()
            .unwrap()
    }
    fn automation(&self, trigger: &str, actions: Value) -> Value {
        json!({"name": "Thanks", "trigger": trigger,
            "conditions": {"account_ids": [self.account]}, "actions": actions})
    }
}

#[tokio::test]
#[serial]
async fn without_approval_nothing_is_read_or_saved_and_the_test_says_why() {
    let e = env(false, page()).await;
    e.published().await;
    let got = e.get("/v1/automations/linkedin").await;
    assert_eq!(got["approved"], false);
    assert_eq!(got["accounts"][0]["status"], "linkedin_needs_approval");
    assert!(got["accounts"][0]["message"]
        .as_str()
        .unwrap()
        .contains("approval"));
    e.tick().await;
    assert_eq!(e.mock.count(), 0, "no LinkedIn call without the approval");
    let body = e.automation(
        "comment_received",
        json!([{"type": "reply_publicly", "text": "Thanks!"}]),
    );
    let res = e.act_on("/v1/automations", body.clone()).await;
    assert_eq!(res.status_code(), 422);
    assert_eq!(res.json::<Value>()["error"], "linkedin_needs_approval");
    // Test this automation sends nothing and says what stops it.
    let test = json!({"automation": body,
        "sample": {"account_id": e.account, "text": "love it", "author_name": "Ann"}});
    let res = e.act_on("/v1/automations/test", test).await;
    assert_eq!(res.status_code(), 200, "{}", res.text());
    let out = res.json::<Value>();
    assert_eq!(out["blocks"], json!(["linkedin_needs_approval"]));
    assert_eq!(out["steps"][0]["text"], "Thanks!");
    assert_eq!(e.mock.count(), 0);
}

#[tokio::test]
#[serial]
async fn a_personal_profile_and_a_page_without_scopes_are_told_apart() {
    let e = env(true, json!({"scopes": ["w_member_social"]})).await;
    let status = |v: &Value| v["accounts"][0]["status"].clone();
    assert_eq!(
        status(&e.get("/v1/automations/linkedin").await),
        "linkedin_personal_profile"
    );
    let body = e.automation(
        "comment_received",
        json!([{"type": "reply_publicly", "text": "Hi"}]),
    );
    let res = e.act_on("/v1/automations", body.clone()).await;
    assert_eq!(res.json::<Value>()["error"], "linkedin_personal_profile");
    e.sql(&format!(
        "UPDATE social_accounts SET meta = '{}' WHERE id = '{}'",
        json!({"organization": ORG, "scopes": ["r_organization_social_feed"]}),
        e.account
    ))
    .await;
    assert_eq!(
        status(&e.get("/v1/automations/linkedin").await),
        "linkedin_needs_scope"
    );
    let res = e.act_on("/v1/automations", body.clone()).await;
    assert_eq!(res.json::<Value>()["error"], "linkedin_needs_scope");
    e.sql(&format!(
        "UPDATE social_accounts SET meta = '{}' WHERE id = '{}'",
        page(),
        e.account
    ))
    .await;
    assert_eq!(status(&e.get("/v1/automations/linkedin").await), "ready");
    let res = e.act_on("/v1/automations", body).await;
    assert_eq!(res.status_code(), 201, "{}", res.text());
    // What LinkedIn never offers apps is refused with its own reasons.
    for (trigger, actions, why) in [
        (
            "dm_received",
            json!([{"type": "send_dm", "text": "hi"}]),
            "linkedin_unsupported_trigger",
        ),
        (
            "mention_received",
            json!([{"type": "reply_publicly", "text": "hi"}]),
            "linkedin_unsupported_trigger",
        ),
        (
            "comment_received",
            json!([{"type": "send_dm", "text": "hi"}]),
            "linkedin_no_dm",
        ),
        (
            "comment_received",
            json!([{"type": "hide_comment"}]),
            "linkedin_no_hide",
        ),
    ] {
        let res = e
            .act_on("/v1/automations", e.automation(trigger, actions))
            .await;
        assert_eq!(res.json::<Value>()["error"], why, "{trigger}");
    }
}

#[tokio::test]
#[serial]
async fn the_poll_reads_a_page_stores_history_as_automated_and_answers_new_comments() {
    let e = env(true, page()).await;
    e.published().await;
    *e.mock.comments.lock().unwrap() = vec![
        comment(1, "urn:li:person:ann", "first!", 120),
        comment(2, ORG, "our own reply", 100),
    ];
    let body = e.automation(
        "comment_received",
        json!([{"type": "reply_publicly", "text": "Thanks {name}!"}, {"type": "like_comment"}]),
    );
    let res = e.act_on("/v1/automations", body).await;
    assert_eq!(res.status_code(), 201);
    let id = res.json::<Value>()["id"].as_str().unwrap().to_owned();
    e.tick().await;
    let items = e.items().await;
    assert_eq!(items.len(), 1, "the Page's own comment is not stored");
    assert!(items[0].automated_at.is_some(), "the first read is history");
    assert_eq!(items[0].root_remote_id.as_deref(), Some(POST));
    assert_eq!(items[0].author_id, "ann");
    assert_eq!(
        e.mock
            .seen(
                "GET",
                "/rest/socialActions/urn%3Ali%3Ashare%3A7001/comments"
            )
            .len(),
        1
    );
    assert!(
        e.mock.seen("POST", "").is_empty(),
        "history is never answered"
    );
    // A comment that arrives later is answered once, in public, as the Page, and liked.
    e.mock
        .comments
        .lock()
        .unwrap()
        .push(comment(3, "urn:li:person:bob", "great post", 0));
    e.tick().await;
    assert_eq!(e.items().await.len(), 2);
    let sent = e.mock.seen("POST", "/rest/socialActions/");
    let runs = e.get(&format!("/v1/automations/{id}/runs")).await;
    assert_eq!(sent.len(), 1, "{sent:?} {runs}");
    assert!(sent[0]
        .0
        .contains("urn%3Ali%3Acomment%3A%28urn%3Ali%3Aactivity%3A7001%2C3%29"));
    assert_eq!(sent[0].1["actor"], ORG);
    assert_eq!(sent[0].1["object"], POST);
    assert_eq!(
        sent[0].1["parentComment"],
        "urn:li:comment:(urn:li:activity:7001,3)"
    );
    assert!(sent[0].1["message"]["text"]
        .as_str()
        .unwrap()
        .starts_with("Thanks "));
    let liked = e.mock.seen("POST", "/rest/reactions");
    assert_eq!(liked.len(), 1);
    assert!(liked[0].0.contains("actor=urn%3Ali%3Aorganization%3A99"));
    assert_eq!(
        liked[0].1["root"],
        "urn:li:comment:(urn:li:activity:7001,3)"
    );
    assert_eq!(liked[0].1["reactionType"], "LIKE");
    // Reading again within the interval does nothing.
    let before = e.mock.count();
    linkedin_inbox::tick(&e.state).await.unwrap();
    assert_eq!(e.mock.count(), before);
}

#[tokio::test]
#[serial]
async fn a_page_stops_at_its_daily_calls_and_a_profile_never_sends() {
    let e = env(true, page()).await;
    e.published().await;
    e.sql(&format!(
        "UPDATE social_accounts SET meta = meta || jsonb_build_object('linkedin_inbox', \
         jsonb_build_object('day', current_date::text, 'calls', {})) WHERE id = '{}'",
        linkedin_inbox::READ_CALLS,
        e.account
    ))
    .await;
    e.tick().await;
    assert_eq!(e.mock.count(), 0, "the day's calls are spent");
    // A reply needs a ready Page: a personal profile gets a refusal and no call.
    *e.mock.comments.lock().unwrap() = vec![comment(1, "urn:li:person:ann", "hi", 5)];
    e.sql(&format!(
        "UPDATE social_accounts SET meta = meta #- '{{linkedin_inbox}}' WHERE id = '{}'",
        e.account
    ))
    .await;
    e.tick().await;
    let item = e.items().await.remove(0);
    e.sql(&format!(
        "UPDATE social_accounts SET meta = '{{}}' WHERE id = '{}'",
        e.account
    ))
    .await;
    let account = e.model().await;
    let sent = linkedin_inbox::send(&e.state, &account, &item, "hello").await;
    assert!(matches!(
        sent,
        Err(InboxError::Invalid("linkedin_personal_profile"))
    ));
    assert!(e.mock.seen("POST", "").is_empty());
}
