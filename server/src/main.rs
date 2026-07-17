use flick_server::{app, config::Config, db::Db, AppState};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info")),
        )
        .init();

    let config = Config::from_env();
    let db = Db::open(&config.data_dir).expect("open sqlite database");
    let addr = config.addr.clone();
    if config.oidc.is_some() {
        tracing::info!(name = %config.oidc_name, "OIDC SSO enabled (lazy discovery)");
    }
    let state = AppState::new(db, config);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("bind {addr}: {e}"));
    tracing::info!("flick-server listening on {addr}");
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server run");
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
