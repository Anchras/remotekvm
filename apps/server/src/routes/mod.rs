use axum::Json;
use serde_json::Value;

pub mod billing;
pub mod machines;
pub mod me;
pub mod organizations;
pub mod sessions;

pub async fn health_handler() -> Json<Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "timestamp": chrono::Utc::now().to_rfc3339()
    }))
}
