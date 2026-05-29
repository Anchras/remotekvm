use anyhow::Result;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;

use remotekvm_server::auth::workos::WorkOsClient;
use remotekvm_server::config::Config;
use remotekvm_server::create_app;
use remotekvm_server::db::Database;
use remotekvm_server::state::AppState;
use remotekvm_server::websocket::SignalingState;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,remotekvm_server=debug".into()),
        )
        .init();

    info!("remotekvm-server starting up");

    let config = Config::from_env()?;
    info!("configuration loaded");

    let db = Database::connect(&config.database_url).await?;
    info!("database connected");

    db.run_migrations().await?;
    info!("database migrations applied");

    let workos = WorkOsClient::new(
        config.workos_api_key.clone(),
        config.workos_client_id.clone(),
        config.workos_api_base.clone(),
    );
    info!("workos client initialized");

    let signaling = SignalingState::new();

    let state = Arc::new(AppState {
        db,
        workos,
        config,
        signaling,
    });

    let app = create_app(Arc::clone(&state));

    let addr = SocketAddr::from(([0, 0, 0, 0], state.config.port));
    info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Graceful shutdown on SIGTERM / SIGINT
    let shutdown = async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler");
        let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("Failed to install SIGINT handler");

        tokio::select! {
            _ = term.recv() => info!("SIGTERM received, starting graceful shutdown"),
            _ = int.recv() => info!("SIGINT received, starting graceful shutdown"),
        }
    };

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;

    info!("server shut down gracefully");
    Ok(())
}
