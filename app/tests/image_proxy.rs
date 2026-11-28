//! `GET /v1/public/image`: a public page's image fetched by brain for the visitor, so the
//! author's host never sees the reader. Request tests against a real Postgres at `DATABASE_URL`
//! (see README) and one loopback origin that plays every kind of host.
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Redirect,
    routing::get,
    Router,
};
use axum_test::TestServer;
use object_store::memory::InMemory;
use sea_orm::DatabaseConnection;
use serial_test::serial;
use socialreins_brain::{
    config::Config, connect, controllers, media::Media, rate_limit::RateLimiter, state::AppState,
};

/// A PNG signature and a few bytes: `sniff` reads the type from the first eight.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";

#[derive(Clone)]
struct Origin {
    port: u16,
    /// The headers of each request the origin saw, in order.
    seen: Arc<Mutex<Vec<HeaderMap>>>,
    calls: Arc<AtomicUsize>,
}

async fn origin() -> Origin {
    let seen: Arc<Mutex<Vec<HeaderMap>>> = Arc::default();
    let calls: Arc<AtomicUsize> = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mock = Origin {
        port,
        seen: seen.clone(),
        calls: calls.clone(),
    };
    let app = Router::new()
        .route("/image.png", get(image))
        .route("/echo.png", get(echo))
        .route(
            "/html",
            get(|| async { ([("content-type", "text/html")], "<html>hi</html>") }),
        )
        .route(
            "/liar.png",
            get(|| async { ([("content-type", "image/png")], "<html>hi</html>") }),
        )
        .route("/big.png", get(big))
        .route("/big-stream.png", get(big_stream))
        .route("/to-private.png", get(to_private))
        .with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, app).await });
    mock
}

async fn image(State(m): State<Origin>) -> ([(&'static str, &'static str); 1], &'static [u8]) {
    m.calls.fetch_add(1, Ordering::Relaxed);
    ([("content-type", "image/png")], PNG)
}

async fn echo(
    State(m): State<Origin>,
    headers: HeaderMap,
) -> ([(&'static str, &'static str); 1], &'static [u8]) {
    m.calls.fetch_add(1, Ordering::Relaxed);
    m.seen.lock().unwrap().push(headers);
    ([("content-type", "image/png")], PNG)
}

/// Over the 5 MB cap, and says so: refused before a byte is read.
async fn big() -> ([(&'static str, &'static str); 1], Vec<u8>) {
    (
        [("content-type", "image/png")],
        vec![0x41; 5 * 1024 * 1024 + 1],
    )
}

/// Over the cap with no length at all: refused while it streams.
async fn big_stream() -> ([(&'static str, &'static str); 1], Body) {
    let chunk = vec![0x41; 1024 * 1024];
    let chunks =
        futures_util::stream::iter((0..6).map(move |_| Ok::<_, std::io::Error>(chunk.clone())));
    ([("content-type", "image/png")], Body::from_stream(chunks))
}

/// A redirect to a private address, the hostname itself private: `PublicOnly` refuses it.
async fn to_private(State(m): State<Origin>) -> Redirect {
    Redirect::temporary(&format!("http://localhost:{}/image.png", m.port))
}

/// A fresh app over the migrated test database, its media pointed at loopback and a small limit.
async fn server(image_limit: u32) -> (TestServer, DatabaseConnection) {
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    config.git_commit = "dev".into();
    // `allow_private` is tests only (never from the environment): it lets the mock origin on
    // loopback through, and a redirect to it that is not the same public rule is still refused.
    config.media = Media::new(Arc::new(InMemory::new()), true);
    let db = connect(&config).await.expect("database reachable");
    let mut state = AppState::new(db.clone(), &config);
    // The real cap is 120 a minute per IP; a low one keeps the test quick and the bucket its own.
    state.image_limiter = Arc::new(RateLimiter::per_minute(
        format!("image-test-{}", uuid::Uuid::now_v7()),
        image_limit,
    ));
    let app = controllers::router(state, &config);
    (TestServer::new(app).unwrap(), db)
}

/// `s` percent-encoded the way a browser sends a query value.
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn ask(base: &str, path: &str) -> String {
    format!("/v1/public/image?url={}", enc(&format!("{base}{path}")))
}

#[tokio::test]
#[serial]
async fn an_image_is_served_from_the_origin_with_its_type_and_a_cache() {
    let (s, _) = server(10).await;
    let o = origin().await;
    let res = s
        .get(&ask(&format!("http://127.0.0.1:{}", o.port), "/image.png"))
        .await;
    res.assert_status_ok();
    assert_eq!(res.header("content-type"), "image/png");
    assert_eq!(
        res.header("cache-control"),
        "public, max-age=3600, immutable"
    );
    assert_eq!(res.header("x-content-type-options"), "nosniff");
    assert_eq!(res.as_bytes(), PNG);
}

#[tokio::test]
#[serial]
async fn a_page_or_a_body_that_is_not_an_image_is_refused() {
    let (s, _) = server(10).await;
    let o = origin().await;
    let base = format!("http://127.0.0.1:{}", o.port);
    let html = s.get(&ask(&base, "/html")).await;
    html.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    html.assert_json(&serde_json::json!({"error": "not_an_image"}));
    // The header says image; the bytes say page.
    let liar = s.get(&ask(&base, "/liar.png")).await;
    liar.assert_status(StatusCode::UNPROCESSABLE_ENTITY);
    liar.assert_json(&serde_json::json!({"error": "not_an_image"}));
}

#[tokio::test]
#[serial]
async fn a_redirect_to_a_private_address_is_refused() {
    let (s, _) = server(10).await;
    let o = origin().await;
    let res = s
        .get(&ask(
            &format!("http://127.0.0.1:{}", o.port),
            "/to-private.png",
        ))
        .await;
    res.assert_status(StatusCode::BAD_REQUEST);
    res.assert_json(&serde_json::json!({"error": "invalid_url"}));
}

#[tokio::test]
#[serial]
async fn a_body_over_the_cap_is_cut_off() {
    let (s, _) = server(10).await;
    let o = origin().await;
    let base = format!("http://127.0.0.1:{}", o.port);
    // Told up front, and only found while it streams.
    for path in ["/big.png", "/big-stream.png"] {
        let res = s.get(&ask(&base, path)).await;
        res.assert_status(StatusCode::PAYLOAD_TOO_LARGE);
        res.assert_json(&serde_json::json!({"error": "image_too_large"}));
    }
}

#[tokio::test]
#[serial]
async fn the_limit_is_counted_per_client() {
    let (s, _) = server(2).await;
    let o = origin().await;
    let url = ask(&format!("http://127.0.0.1:{}", o.port), "/image.png");
    for _ in 0..2 {
        s.get(&url).await.assert_status_ok();
    }
    let refused = s.get(&url).await;
    refused.assert_status(StatusCode::TOO_MANY_REQUESTS);
    refused.assert_json(&serde_json::json!({"error": "rate_limited"}));
    // The refused request never reached the origin.
    assert_eq!(o.calls.load(Ordering::Relaxed), 2);
}

#[tokio::test]
#[serial]
async fn no_cookie_or_auth_header_of_the_reader_reaches_the_origin() {
    let (s, _) = server(10).await;
    let o = origin().await;
    let res = s
        .get(&ask(&format!("http://127.0.0.1:{}", o.port), "/echo.png"))
        .add_header("cookie", "session=secret")
        .add_header("authorization", "Bearer secret")
        .add_header("referer", "https://socialreins.com/p/abcdefghjk")
        .await;
    res.assert_status_ok();
    assert_eq!(o.calls.load(Ordering::Relaxed), 1);
    let seen = o.seen.lock().unwrap();
    let headers = seen.last().unwrap();
    for name in ["cookie", "authorization", "referer"] {
        assert!(
            headers.get(name).is_none(),
            "{name} was forwarded: {headers:?}"
        );
    }
    // Only what the proxy itself sets went out.
    assert_eq!(
        headers.get("accept").and_then(|v| v.to_str().ok()),
        Some("image/*")
    );
}

/// The endpoint never takes a URL that is not a public https one: a plain http address, a private
/// literal or a malformed one never opens a connection.
#[tokio::test]
#[serial]
async fn only_a_public_https_url_is_fetched() {
    let mut config = Config::from_env().expect("DATABASE_URL must point at a test Postgres");
    config.git_commit = "dev".into();
    // As in production: no loopback, no plain http.
    config.media = Media::new(Arc::new(InMemory::new()), false);
    let db = connect(&config).await.expect("database reachable");
    let state = AppState::new(db, &config);
    let s = TestServer::new(controllers::router(state, &config)).unwrap();
    for url in [
        "http://example.com/a.png",
        "https://127.0.0.1/a.png",
        "https://localhost/a.png",
        "https://user:pw@example.com/a.png",
        "not a url",
        "",
    ] {
        let res = s.get(&format!("/v1/public/image?url={}", enc(url))).await;
        res.assert_status(StatusCode::BAD_REQUEST);
        res.assert_json(&serde_json::json!({"error": "invalid_url"}));
    }
}
