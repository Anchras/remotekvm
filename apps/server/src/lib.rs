//! RemoteKVM coordination server.
//!
//! This crate is split into a library (everything in `src/`) and a thin binary
//! (`src/main.rs`) so that integration tests in `tests/` can build the Axum
//! application in-process against an isolated test database.

use axum::{
    middleware,
    routing::{delete, get, post},
    Router,
};
use std::sync::Arc;

pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod routes;
pub mod state;
pub mod util;
pub mod websocket;

use auth::middleware::jwt_auth_middleware;
use state::AppState;

/// Build the full Axum router for the server.
///
/// Split out of `main` so both the binary and the integration tests construct
/// the exact same application.
pub fn create_app(state: Arc<AppState>) -> Router {
    use tower_http::trace::TraceLayer;

    // Public routes (no auth required)
    let public_routes = Router::new()
        .route("/health", get(routes::health_handler))
        .route("/auth/login", get(auth::routes::login_init))
        .route("/auth/workos/callback", get(auth::routes::workos_callback))
        // Stripe verifies authenticity via the signature header, so this is public.
        .route("/webhooks/stripe", post(routes::billing::stripe_webhook));

    // Protected API routes (JWT auth required)
    let api_routes = Router::new()
        .route("/api/me", get(routes::me::me_handler))
        .route("/api/machines", get(routes::machines::list_machines))
        .route("/api/machines", post(routes::machines::register_machine))
        .route("/api/machines/:id", get(routes::machines::get_machine))
        .route(
            "/api/machines/:id",
            delete(routes::machines::delete_machine),
        )
        .route(
            "/api/machines/:id/rotate-token",
            post(routes::machines::rotate_token),
        )
        .route(
            "/api/machines/:id/connect",
            post(routes::sessions::connect_machine),
        )
        .route(
            "/api/billing/checkout",
            post(routes::billing::create_checkout),
        )
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
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
