use axum::{
    extract::{Extension, Path, State},
    response::Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::auth::Claims;
use crate::auth::routes::AppState;
use crate::error::ApiError;

/// POST /api/machines/{id}/connect — Request a connection to a machine.
pub async fn connect_machine(
    Extension(claims): Extension<Claims>,
    Path(machine_id): Path<uuid::Uuid>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ConnectResponse>, ApiError> {
    let user_id = uuid::Uuid::parse_str(&claims.sub)
        .map_err(|_| ApiError::Internal("Invalid user ID".to_string()))?;

    // Verify machine exists and user has access
    let machine = sqlx::query_as::<_, MachineStatusRow>(
        r#"
        SELECT m.id, m.online
        FROM machines m
        WHERE m.id = $1
          AND (m.user_id = $2 OR m.organization_id IN (
              SELECT organization_id FROM organization_members WHERE user_id = $2
          ))
        "#
    )
    .bind(machine_id)
    .bind(user_id)
    .fetch_optional(state.db.pool())
    .await?
    .ok_or_else(|| ApiError::NotFound("Machine not found".to_string()))?;

    if !machine.online {
        return Err(ApiError::BadRequest("Machine is offline".to_string()));
    }

    // Create a session record
    let session_id = uuid::Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO sessions (id, user_id, machine_id, status)
        VALUES ($1, $2, $3, 'pending')
        "#
    )
    .bind(session_id)
    .bind(user_id)
    .bind(machine_id)
    .execute(state.db.pool())
    .await?;

    // TODO: Notify agent via WebSocket about the connection request
    // This requires the signaling relay to be implemented

    Ok(Json(ConnectResponse {
        session_id: session_id.to_string(),
        status: "pending".to_string(),
    }))
}

#[derive(Debug, Serialize)]
pub struct ConnectResponse {
    pub session_id: String,
    pub status: String,
}

#[derive(sqlx::FromRow)]
struct MachineStatusRow {
    id: uuid::Uuid,
    online: bool,
}
