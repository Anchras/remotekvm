use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use std::sync::Arc;

use crate::state::AppState;

pub mod billing;
pub mod machines;
pub mod me;
pub mod sessions;

pub async fn health_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let timestamp = chrono::Utc::now().to_rfc3339();
    let db_ok = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(state.db.pool())
        .await
        .is_ok();

    if db_ok {
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
                "timestamp": timestamp,
                "dependencies": {
                    "database": "ok"
                }
            })),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "degraded",
                "version": env!("CARGO_PKG_VERSION"),
                "timestamp": timestamp,
                "dependencies": {
                    "database": "unavailable"
                }
            })),
        )
    }
}
