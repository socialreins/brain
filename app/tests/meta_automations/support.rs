//! A mock Graph API, a signed-in team with a Facebook Page, and helpers to feed items to the
//! automations engine and read back what it sent.
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
use socialreins_brain::{
    config::Config,
    connect,
    connectors::{bluesky::Author, meta::Meta},
    controllers,
    inbox::{self, Fresh},
    models::{inbox_items, magic_links, social_accounts},
    state::AppState,
};
use socialreins_core::token;
use uuid::Uuid;

pub const WEB: &str = "http://localhost:3000";

/// What the mock does with a POST: 0 takes it, 1 answers Meta's rate limit (613), 2 answers a
/// policy refusal (200).
#[derive(Default)]
pub struct Mock {
    pub mode: AtomicU8,
    /// (path, form body) of every POST taken, oldest first.
    pub posts: Mutex<Vec<(String, String)>>,
    /// What the mock AI model answers, and how many times it was asked.
    pub ai_reply: Mutex<String>,
    pub ai_calls: std::sync::atomic::AtomicUsize,
}

impl Mock {
    pub fn sent(&self) -> usize {
        self.posts.lock().unwrap().len()
    }
    /// The POSTs to `/{id}/messages` (private replies and DMs) and `/{id}/comments`.
    pub fn to(&self, tail: &str) -> Vec<String> {
        let posts = self.posts.lock().unwrap();
        let hit = posts.iter().filter(|p| p.0.ends_with(tail));
        hit.map(|p| p.1.clone()).collect()
    }
}

async fn handler(State(m): State<Arc<Mock>>, method: Method, uri: Uri, body: Bytes) -> Response {
    if method != Method::POST {
        return StatusCode::NOT_FOUND.into_response();
    }
    if uri.path() == "/v1/messages" {
        m.ai_calls.fetch_add(1, Ordering::SeqCst);
        let text = m.ai_reply.lock().unwrap().clone();
        return Json(json!({"content": [{"type": "text", "text": text}]})).into_response();
    }
    let form = String::from_utf8_lossy(&body).into_owned();
    m.posts.lock().unwrap().push((uri.path().to_owned(), form));
    let refused = |code: i64| {
        let body = json!({"error": {"message": "mock refusal", "code": code}});
        (StatusCode::BAD_REQUEST, Json(body)).into_response()
    };
    match m.mode.load(Ordering::SeqCst) {
        1 => refused(613),
        2 => refused(200),
        _ => {
            Json(json!({"id": "9_9", "message_id": "mid.1", "recipient_id": "777"})).into_response()
        }
    }
}

pub struct Env {
    pub s: TestServer,
    pub db: DatabaseConnection,
    pub state: AppState,
    pub cookie: String,
    pub mock: Arc<Mock>,
    pub account: social_accounts::Model,
    pub team: Uuid,
}

pub async fn env() -> Env {
    env_with(Opts::default()).await
}

#[derive(Default, Clone, Copy)]
pub struct Opts {
    /// `META_HUMAN_AGENT`.
    pub human_agent: bool,
    /// Our own AI key is set (the mock model answers with `Mock::ai_reply`).
    pub ai_key: bool,
}

pub async fn env_with(opts: Opts) -> Env {
    let mock = Arc::new(Mock::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new().fallback(handler).with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    config.trust_proxy = true;
    let mut meta = Meta::new(Some(("mapp".into(), "msecret".into())), "v26.0").unwrap();
    meta.graph_url = base.clone();
    meta.human_agent = opts.human_agent;
    config.meta = meta;
    config.ai.own_key = opts
        .ai_key
        .then(|| "sk-ant-api03-oursoursoursoursoursours".into());
    config.ai.anthropic_url = base.clone();
    let db = connect(&config).await.expect("database reachable");
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
    let account = new_account(&state, &db, team, "facebook", &digits()).await;
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

/// A connected account made an hour ago, so items from now count (`item.received_at` must not
/// be before it).
pub async fn new_account(
    state: &AppState,
    db: &DatabaseConnection,
    team: Uuid,
    provider: &str,
    external: &str,
) -> social_accounts::Model {
    let id = Uuid::now_v7();
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "INSERT INTO social_accounts (id, team_id, provider, external_id, handle, access_token, \
         token_expires_at, meta, created_at) VALUES ($1, $2, $3, $4, 'page', $5, \
         now() + interval '30 days', $6, now() - interval '1 hour')",
        vec![
            id.into(),
            team.into(),
            provider.to_owned().into(),
            external.to_owned().into(),
            state.cipher.encrypt(b"pt").into(),
            json!({"page_id": external}).into(),
        ],
    ))
    .await
    .unwrap();
    social_accounts::Entity::find_by_id(id)
        .one(db)
        .await
        .unwrap()
        .unwrap()
}

impl Env {
    pub async fn sql(&self, sql: &str) {
        self.db.execute_unprepared(sql).await.unwrap();
    }

    pub async fn post(&self, path: &str, body: Value) -> axum_test::TestResponse {
        self.s
            .post(path)
            .add_header("origin", WEB)
            .add_header("cookie", self.cookie.clone())
            .json(&body)
            .await
    }

    pub async fn patch(&self, path: &str, body: Value) -> axum_test::TestResponse {
        self.s
            .patch(path)
            .add_header("origin", WEB)
            .add_header("cookie", self.cookie.clone())
            .json(&body)
            .await
    }

    /// Creates an automation limited to the Page; its id.
    pub async fn automation(&self, mut body: Value) -> String {
        body["conditions"]["account_ids"] = json!([self.account.id]);
        let res = self.post("/v1/automations", body).await;
        assert_eq!(res.status_code(), 201, "{}", res.text());
        res.json::<Value>()["id"].as_str().unwrap().to_owned()
    }

    /// Stores an item `age_minutes` old and runs the automations over it, as a webhook does.
    pub async fn item(
        &self,
        kind: &'static str,
        author: &str,
        text: &str,
        age_minutes: i64,
    ) -> inbox_items::Model {
        // Graph ids are digits and underscores.
        let remote = format!("101_{}", Uuid::now_v7().as_u128() % 1_000_000_000_000);
        let fresh = Fresh {
            kind,
            remote_id: remote,
            remote_cid: None,
            parent: (kind == "comment").then(|| "101_post".to_owned()),
            root: ((kind == "comment").then(|| "101_post".to_owned()), None),
            convo_id: (kind == "dm").then(|| author.to_owned()),
            author: Author {
                did: author.to_owned(),
                handle: None,
                display_name: Some("Ada".into()),
                avatar: None,
            },
            text: text.to_owned(),
            at: (Utc::now() - Duration::minutes(age_minutes)).fixed_offset(),
            permalink: None,
            story: None,
            button_payload: None,
            attachments: Vec::new(),
        };
        let item = inbox::store(&self.db, &self.account, fresh)
            .await
            .unwrap()
            .expect("a new item");
        socialreins_brain::automations::run(&self.state, &self.account, &item)
            .await
            .unwrap();
        item
    }

    /// Hits in the last day of a rate limit bucket.
    pub async fn count_hits(&self, bucket: &str) -> i64 {
        let sql = format!(
            "SELECT count(*) AS n FROM rate_limit_hits WHERE bucket = '{bucket}' \
             AND created_at > now() - interval '1 day'"
        );
        let r = self
            .db
            .query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
            .await
            .unwrap()
            .unwrap();
        r.try_get("", "n").unwrap()
    }

    /// (status, reason) of the item's runs of the automation.
    pub async fn run(&self, automation: &str, item: Uuid) -> Option<(String, Option<String>)> {
        let sql = format!(
            "SELECT status, reason FROM automation_runs WHERE automation_id = '{automation}' \
             AND inbox_item_id = '{item}'"
        );
        let r = self
            .db
            .query_one_raw(Statement::from_string(DbBackend::Postgres, sql))
            .await
            .unwrap()?;
        Some((
            r.try_get("", "status").unwrap(),
            r.try_get("", "reason").unwrap(),
        ))
    }
}

/// A comment automation that replies publicly, `extra` merged into its body.
pub fn reply_all(extra: Value) -> Value {
    let mut body = json!({"name": "Thanks", "trigger": "comment_received",
        "actions": [{"type": "reply_publicly", "text": "Thanks {name}!"}]});
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    body
}

/// A fresh Graph-style id: accounts and people outlive a test in the shared database.
pub fn digits() -> String {
    (Uuid::now_v7().as_u128() % 1_000_000_000_000_000).to_string()
}
