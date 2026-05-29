use axum::{
    extract::{Extension, Path, State},
    response::Json,
};
use serde::Serialize;
use std::sync::Arc;

use crate::auth::Claims;
use crate::error::ApiError;
use crate::state::AppState;

/// POST /api/machines/{id}/connect — Request a connection to a machine.
pub async fn connect_machine(
    Extension(claims): Extension<Claims>,
    Path(machine_id): Path<uuid::Uuid>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ConnectResponse>, ApiError> {
    let user_id = claims
        .user_id()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    // Verify the machine exists and the user has access.
    sqlx::query_scalar::<_, uuid::Uuid>(
        r#"
        SELECT m.id
        FROM machines m
        WHERE m.id = $1
          AND (m.user_id = $2 OR m.organization_id IN (
              SELECT organization_id FROM organization_members WHERE user_id = $2
          ))
        "#,
    )
    .bind(machine_id)
    .bind(user_id)
    .fetch_optional(state.db.pool())
    .await?
    .ok_or_else(|| ApiError::NotFound("Machine not found".to_string()))?;

    // Authoritative liveness check: the in-memory signaling map is the source of
    // truth for "can we reach this agent right now". We deliberately do NOT gate
    // on the `machines.online` DB column — that flag is updated from concurrent
    // agent connect/disconnect handlers and can briefly be stale (e.g. a
    // reconnecting agent racing the old socket's offline write), whereas the map
    // always reflects the live socket.
    if !state
        .signaling
        .is_agent_online(&machine_id.to_string())
        .await
    {
        return Err(ApiError::BadRequest(
            "Machine agent is not connected to the signaling server".to_string(),
        ));
    }

    // Provision a pending session. The actual WebRTC negotiation happens over
    // the signaling WebSocket: the client connects to `/client`, sends a
    // `ConnectRequest { session_id, offer }`, and the relay (see `websocket.rs`)
    // verifies ownership of this session row before forwarding the offer to the
    // agent. The REST endpoint exists to authorize and record the session up
    // front, not to carry SDP.
    let session_id = uuid::Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO sessions (id, user_id, machine_id, status)
        VALUES ($1, $2, $3, 'pending')
        "#,
    )
    .bind(session_id)
    .bind(user_id)
    .bind(machine_id)
    .execute(state.db.pool())
    .await?;

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
