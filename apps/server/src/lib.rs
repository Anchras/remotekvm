//! RemoteKVM coordination server.
//!
//! This crate is split into a library (everything in `src/`) and a thin binary
//! (`src/main.rs`) so that integration tests in `tests/` can build the Axum
//! application in-process against an isolated test database.

use axum::{
    extract::State,
    http::{header, Request, StatusCode},
    middleware,
    response::IntoResponse,
    routing::put,
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

    let auth_routes = Router::new()
        .route("/auth/login", get(auth::routes::login_init))
        .route("/auth/workos/callback", get(auth::routes::workos_callback))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth_rate_limit,
        ));

    // Public routes (no auth required)
    let public_routes = Router::new()
        .route("/health", get(routes::health_handler))
        // Stripe verifies authenticity via the signature header, so this is public.
        .route("/webhooks/stripe", post(routes::billing::stripe_webhook))
        .merge(auth_routes);

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
        .route("/api/sessions", get(routes::sessions::list_sessions))
        .route(
            "/api/organizations",
            get(routes::organizations::list_organizations),
        )
        .route(
            "/api/organizations",
            post(routes::organizations::create_organization),
        )
        .route(
            "/api/organizations/:id/members",
            get(routes::organizations::list_members),
        )
        .route(
            "/api/organizations/:id/invite",
            post(routes::organizations::invite_member),
        )
        .route(
            "/api/organizations/:id/machines/:machine_id",
            put(routes::organizations::share_machine),
        )
        .route(
            "/api/billing/checkout",
            post(routes::billing::create_checkout),
        )
        .route("/api/billing/usage", get(routes::billing::usage))
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

async fn auth_rate_limit(
    State(state): State<Arc<AppState>>,
    req: Request<axum::body::Body>,
    next: middleware::Next,
) -> impl IntoResponse {
    let key = req
        .headers()
        .get("x-forwarded-for")
        .or_else(|| req.headers().get("x-real-ip"))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
        .or_else(|| {
            req.headers()
                .get(header::USER_AGENT)
                .and_then(|value| value.to_str().ok())
                .map(|value| format!("ua:{value}"))
        })
        .unwrap_or_else(|| "anonymous".to_string());

    if !state.auth_rate_limiter.check(&key) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "too many authentication attempts",
        )
            .into_response();
    }

    next.run(req).await.into_response()
}
