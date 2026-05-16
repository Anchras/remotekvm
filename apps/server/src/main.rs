use anyhow::Result;
use axum::{
    routing::get,
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;

mod auth;
mod config;
mod db;
mod error;
mod routes;
mod websocket;

use auth::routes::AppState;
use auth::workos::WorkOsClient;
use config::Config;
use db::Database;

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

    // Run migrations
    db.run_migrations().await?;
    info!("database migrations applied");

    let workos = WorkOsClient::new(
        config.workos_api_key.clone(),
        config.workos_client_id.clone(),
    );
    info!("workos client initialized");

    let state = Arc::new(AppState {
        db,
        workos,
        config,
    });

    let app = create_app(Arc::clone(&state));

    let addr = SocketAddr::from(([0, 0, 0, 0], state.config.port));
    info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn create_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(routes::health_handler))
        .route("/auth/workos/callback", get(auth::routes::workos_callback))
        .with_state(state)
}
