use anyhow::Result;
use axum::{
    routing::get,
    Router,
};
use std::net::SocketAddr;
use tracing::info;

mod config;
mod db;
mod error;
mod routes;
mod websocket;

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

    let app = create_app(db);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn create_app(db: Database) -> Router {
    Router::new()
        .route("/health", get(routes::health_handler))
        .with_state(db)
}
