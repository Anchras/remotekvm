// remotekvm-server
//
// SaaS coordination server for RemoteKVM.
//
// Responsibilities:
//   - OAuth2 authentication (Google, GitHub, Microsoft)
//   - JWT session management
//   - Machine registration and discovery
//   - WebRTC signaling relay (WebSocket)
//   - REST API for dashboard data
//   - Stripe billing webhooks
//
// Phase 0 MVP:
//   - Health endpoint
//   - Basic Axum server scaffolding
//   - WebSocket signaling structure
//   - Placeholder auth middleware

use anyhow::Result;
use axum::{
    routing::get,
    Router,
};
use std::net::SocketAddr;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,remotekvm_server=debug".into()),
        )
        .init();

    info!("remotekvm-server starting up");

    let app = Router::new()
        .route("/health", get(health_handler));

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn health_handler() -> &'static str {
    "OK"
}
