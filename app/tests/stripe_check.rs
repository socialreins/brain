//! The daily Stripe check against a mock Stripe: it lists, never writes, finds each kind of
//! difference with the books, does nothing with billing off, and says when Stripe cannot be
//! read. Needs `DATABASE_URL` like the request tests.
use std::sync::{Arc, Mutex};

use axum::{
    extract::{Query, State},
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use chrono::Utc;
use sea_orm::ConnectionTrait;
use serde_json::{json, Value};
use serial_test::serial;
use socialreins_brain::{
    config::Config,
    connect,
    state::AppState,
    stripe::Stripe,
    stripe_check::{self, Mismatch},
};
use uuid::Uuid;

#[derive(Default)]
struct Mock {
    /// Every call: method and `starting_after`.
    calls: Mutex<Vec<(String, String)>>,
    subs: Mutex<Vec<Value>>,
    refuse: Mutex<bool>,
}

async fn list(
    State(m): State<Arc<Mock>>,
    method: Method,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let get = |k: &str| q.get(k).cloned().unwrap_or_default();
    m.calls
        .lock()
        .unwrap()
        .push((method.to_string(), get("starting_after")));
    if *m.refuse.lock().unwrap() {
        let body = json!({"error": {"code": "api_key_expired"}});
        return (StatusCode::UNAUTHORIZED, Json(body)).into_response();
    }
    let subs = m.subs.lock().unwrap();
    let all: Vec<&Value> = subs.iter().collect();
    let from = all
        .iter()
        .position(|s| s["id"] == get("starting_after"))
        .map_or(0, |i| i + 1);
    let limit: usize = get("limit").parse().unwrap_or(10);
    let page: Vec<&Value> = all.iter().skip(from).take(limit).copied().collect();
    let more = from + page.len() < all.len();
    Json(json!({"data": page, "has_more": more})).into_response()
}

async fn mock() -> (Arc<Mock>, String) {
    let m = Arc::new(Mock::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/v1/subscriptions", any(list))
        .with_state(m.clone());
    tokio::spawn(async move { axum::serve(listener, router).await });
    (m, base)
}

fn stripe_at(base: &str) -> Stripe {
    let mut s = Stripe::new(
        Some("sk_test_1".into()),
        None,
        [
            ("scheduler", "month", Some("price_s".into())),
            ("automation", "month", Some("price_a".into())),
            ("scheduler", "year", Some("price_sy".into())),
            ("automation", "year", Some("price_ay".into())),
        ],
        [("x100", None), ("x500", None)],
    )
    .unwrap();
    s.api_url = base.to_owned();
    s
}

fn sub(id: &str, status: &str, price: &str, team: Option<Uuid>, age_minutes: i64) -> Value {
    let created = Utc::now().timestamp() - age_minutes * 60;
    json!({"id": id, "status": status, "created": created,
        "metadata": team.map_or(json!({}), |t| json!({"team_id": t.to_string()})),
        "items": {"data": [{"price": {"id": price}}]}})
}

async fn state(base: &str, billing: bool) -> AppState {
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    config.billing_enabled = billing;
    config.stripe = stripe_at(base);
    let db = connect(&config).await.expect("database reachable");
    AppState::new(db, &config)
}

/// A team with a Stripe subscription row `age_minutes` old.
async fn team(app: &AppState, row: Option<(&str, &str, &str, i64)>) -> Uuid {
    let id = Uuid::now_v7();
    let code: String = id
        .as_u128()
        .to_string()
        .bytes()
        .rev()
        .take(6)
        .map(|d| (b'p' + d % 10) as char)
        .collect();
    app.db
        .execute_unprepared(&format!(
            "INSERT INTO teams (id, name, code) VALUES ('{id}', 't', '{code}')"
        ))
        .await
        .unwrap();
    if let Some((sub, status, plan, age)) = row {
        app.db
            .execute_unprepared(&format!(
                "INSERT INTO subscriptions (team_id, plan_id, status, provider, \
                 provider_subscription_id, updated_at) VALUES ('{id}', '{plan}', '{status}', \
                 'stripe', '{sub}', now() - interval '{age} minutes')"
            ))
            .await
            .unwrap();
    }
    id
}

fn said(found: &[Mismatch], team: Uuid) -> Vec<String> {
    found
        .iter()
        .filter(|m| m.team == Some(team))
        .map(|m| m.what.clone())
        .collect()
}

#[tokio::test]
#[serial]
async fn it_lists_pages_and_only_reads() {
    let (m, base) = mock().await;
    *m.subs.lock().unwrap() = (0..230)
        .map(|i| sub(&format!("sub_{i:03}"), "active", "price_s", None, 500))
        .collect();
    let all = stripe_at(&base).subscriptions().await.unwrap();
    assert_eq!(all.len(), 230);
    assert_eq!(all[229]["id"], "sub_229");
    let calls = m.calls.lock().unwrap();
    assert_eq!(calls.len(), 3, "three pages of 100");
    assert_eq!(calls[1].1, "sub_099");
    assert!(calls.iter().all(|c| c.0 == "GET"));
}

#[tokio::test]
#[serial]
async fn each_difference_is_found_and_nothing_is_written() {
    let (m, base) = mock().await;
    let app = state(&base, true).await;
    // What an earlier run of this test left.
    app.db
        .execute_unprepared(
            "DELETE FROM subscriptions WHERE provider_subscription_id LIKE 'sub_chk_%'",
        )
        .await
        .unwrap();
    let (same, status, gone, fresh, plan) = (
        team(&app, Some(("sub_chk_same", "active", "scheduler", 500))).await,
        team(&app, Some(("sub_chk_status", "active", "scheduler", 500))).await,
        team(&app, Some(("sub_chk_gone", "past_due", "scheduler", 500))).await,
        team(&app, Some(("sub_chk_fresh", "active", "scheduler", 3))).await,
        team(&app, Some(("sub_chk_plan", "trialing", "scheduler", 500))).await,
    );
    let unbooked = team(&app, None).await;
    let trial = team(&app, Some(("sub_chk_trial", "trialing", "scheduler", 500))).await;
    let over = team(&app, None).await;
    *m.subs.lock().unwrap() = vec![
        sub("sub_chk_same", "active", "price_s", Some(same), 500),
        sub("sub_chk_status", "trialing", "price_s", Some(status), 500),
        sub("sub_chk_plan", "trialing", "price_ay", Some(plan), 500),
        sub("sub_chk_unbooked", "active", "price_a", Some(unbooked), 500),
        // Made a minute ago: its webhook may still be on the way.
        sub("sub_chk_new", "active", "price_s", Some(Uuid::now_v7()), 1),
        // Another product on the same Stripe account: no team, no price of ours.
        sub("sub_chk_other", "active", "price_other", None, 500),
        // A trial that ended five minutes ago: Stripe moved it, the webhook may be coming.
        {
            let mut v = sub("sub_chk_trial", "active", "price_s", Some(trial), 5000);
            v["trial_end"] = json!(Utc::now().timestamp() - 300);
            v
        },
        // One that is over is not live whatever it says.
        sub("sub_chk_over", "unpaid", "price_s", Some(over), 500),
    ];
    let before: i64 = rows(&app).await;
    let found = stripe_check::check(&app).await.unwrap().unwrap();
    assert!(said(&found, same).is_empty());
    assert_eq!(
        said(&found, status),
        ["Stripe has it trialing, brain has active"]
    );
    assert_eq!(
        said(&found, gone),
        ["brain has it past_due, Stripe has none live"]
    );
    assert!(said(&found, fresh).is_empty(), "changed 3 minutes ago");
    assert!(said(&found, trial).is_empty(), "trial ended 5 minutes ago");
    assert!(said(&found, over).is_empty(), "unpaid is not live");
    assert_eq!(
        said(&found, plan),
        ["Stripe has plan automation, brain has scheduler"]
    );
    assert_eq!(
        said(&found, unbooked),
        ["Stripe has it active, brain has no row"]
    );
    assert!(found.iter().all(|f| !f.what.contains("sub_")));
    assert_eq!(rows(&app).await, before, "nothing is written");
    let calls = m.calls.lock().unwrap();
    assert!(calls.iter().all(|c| c.0 == "GET"));
    assert_eq!(calls.len(), 1, "one list, not one a status");
}

async fn rows(app: &AppState) -> i64 {
    use sea_orm::{DbBackend, Statement};
    let row = app
        .db
        .query_one_raw(Statement::from_string(
            DbBackend::Postgres,
            "SELECT count(*)::bigint AS n, COALESCE(sum(extract(epoch FROM updated_at)), 0)::bigint AS t FROM subscriptions".to_owned(),
        ))
        .await
        .unwrap()
        .unwrap();
    row.try_get::<i64>("", "n").unwrap() * 1_000_000_000_000 + row.try_get::<i64>("", "t").unwrap()
}

#[tokio::test]
#[serial]
async fn with_billing_off_nothing_is_asked() {
    let (m, base) = mock().await;
    let app = state(&base, false).await;
    assert!(stripe_check::check(&app).await.is_none());
    assert!(m.calls.lock().unwrap().is_empty());
}

#[tokio::test]
#[serial]
async fn a_stripe_that_cannot_be_read_is_said_without_a_key() {
    let (m, base) = mock().await;
    *m.refuse.lock().unwrap() = true;
    let app = state(&base, true).await;
    let why = stripe_check::check(&app).await.unwrap().unwrap_err();
    assert_eq!(why, "Stripe could not be read (api_key_expired)");
    assert!(!why.contains("sk_"));
}
