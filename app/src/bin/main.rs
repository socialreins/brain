use std::process::ExitCode;

use socialreins_brain::{config::Config, connect, controllers, scheduler, state::AppState};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "brain stopped");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let db = connect(&config).await?;
    scheduler::spawn(db.clone());
    let app = controllers::router(AppState::new(db, &config), &config);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.port)).await?;
    let ip_source = if config.trust_proxy {
        "x-forwarded-for, then socket"
    } else {
        "socket"
    };
    tracing::info!(
        port = config.port,
        client_ip_source = ip_source,
        "listening"
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await?;
    Ok(())
}

/// Ctrl-C locally, SIGTERM on Railway: stop accepting, finish in-flight requests.
async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut s) = signal(SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { () = ctrl_c => {}, () = term => {} }
}
