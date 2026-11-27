//! A sign-in mail that could not be sent is counted for the operator digest; one that went out
//! is not. Needs `DATABASE_URL` like the request tests.
use std::{sync::Arc, time::Duration};

use axum::{routing::post, Json, Router};
use axum_test::TestServer;
use chrono::Utc;
use serde_json::{json, Value};
use serial_test::serial;
use socialreins_brain::{
    config::Config,
    connect, controllers,
    mail::{Mailer, SIGNIN_FAILED},
    rate_limit,
    state::AppState,
};
use uuid::Uuid;

async fn server(mailer_at: &str) -> (TestServer, AppState) {
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    config.trust_proxy = true;
    let db = connect(&config).await.expect("database reachable");
    let mut state = AppState::new(db, &config);
    state.mailer = Mailer::resend_at(mailer_at, "re_test", "b@example.com").unwrap();
    let app = controllers::router(state.clone(), &config);
    (TestServer::new(app).unwrap(), state)
}

async fn failed(state: &AppState, since: chrono::DateTime<Utc>) -> i64 {
    let until = Utc::now() + chrono::Duration::minutes(1);
    rate_limit::count_since(&state.db, SIGNIN_FAILED, (since, until))
        .await
        .unwrap()
}

/// Asks for a link from a fresh address (the limiter is per address) and waits for the
/// spawned send to end.
async fn ask(s: &TestServer) {
    let ip = format!(
        "10.{}.{}.1",
        Uuid::now_v7().as_u128() % 250,
        Utc::now().timestamp() % 250
    );
    let email = format!("{}@example.com", Uuid::now_v7());
    let res = s
        .post("/auth/link")
        .add_header("x-forwarded-for", ip)
        .json(&json!({"email": email}))
        .await;
    assert_eq!(res.status_code(), 202);
    assert_eq!(res.json::<Value>(), json!({"status": "sent"}));
}

#[tokio::test]
#[serial]
async fn a_sign_in_mail_that_cannot_be_sent_is_counted_and_a_sent_one_is_not() {
    let since = Utc::now() - chrono::Duration::seconds(1);
    // A mail service nobody answers on: three tries, then it counts as failed.
    let (down, state) = server("http://127.0.0.1:9/emails").await;
    let before = failed(&state, since).await;
    ask(&down).await;
    let mut n = before;
    for _ in 0..60 {
        n = failed(&state, since).await;
        if n > before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(n, before + 1);

    // A working one adds nothing.
    let seen = Arc::new(std::sync::Mutex::new(0));
    let log = seen.clone();
    let fake = Router::new().route(
        "/emails",
        post(move |Json(_): Json<Value>| async move {
            *log.lock().unwrap() += 1;
            Json(json!({"id": "x"}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/emails", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, fake).await });
    let (up, state) = server(&url).await;
    ask(&up).await;
    for _ in 0..60 {
        if *seen.lock().unwrap() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(*seen.lock().unwrap(), 1);
    assert_eq!(failed(&state, since).await, before + 1);
}
