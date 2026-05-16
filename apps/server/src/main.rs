use anyhow::Result;
use axum::{
    middleware,
    routing::{delete, get, post},
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

use auth::middleware::jwt_auth_middleware;
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

    db.run_migrations().await?;
    info!("database migrations applied");

    let workos = WorkOsClient::new(
        config.workos_api_key.clone(),
        config.workos_client_id.clone(),
    );
    info!("workos client initialized");

    let signaling = websocket::SignalingState::new();

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
    axum::serve(listener, app).await?;

    Ok(())
}

fn create_app(state: Arc<AppState>) -> Router {
    // Public routes (no auth required)
    let public_routes = Router::new()
        .route("/health", get(routes::health_handler))
        .route("/auth/workos/callback", get(auth::routes::workos_callback));

    // Protected API routes (JWT auth required)
    let api_routes = Router::new()
        .route("/api/me", get(routes::me::me_handler))
        .route("/api/machines", get(routes::machines::list_machines))
        .route("/api/machines", post(routes::machines::register_machine))
        .route("/api/machines/:id", get(routes::machines::get_machine))
        .route("/api/machines/:id", delete(routes::machines::delete_machine))
        .route("/api/machines/:id/connect", post(routes::sessions::connect_machine))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            jwt_auth_middleware,
        ));

    // WebSocket routes
    let ws_routes = Router::new()
        .route("/agent", get(websocket::agent_ws_handler))
        .route("/client", get(websocket::client_ws_handler));

    public_routes
        .merge(api_routes)
        .merge(ws_routes)
        .with_state(state)
}
