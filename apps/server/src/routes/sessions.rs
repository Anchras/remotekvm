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
    let machine = sqlx::query_as::<_, MachineAccessRow>(
        r#"
        SELECT m.id, m.organization_id, m.user_id, COALESCE(o.plan, 'free') AS plan
        FROM machines m
        LEFT JOIN organizations o ON o.id = m.organization_id
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

    ensure_usage_quota(&state, &machine, user_id).await?;

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
    .bind(machine.id)
    .execute(state.db.pool())
    .await?;

    state
        .signaling
        .record_session_route(
            &session_id.to_string(),
            &machine_id.to_string(),
            &user_id.to_string(),
        )
        .await;

    Ok(Json(ConnectResponse {
        session_id: session_id.to_string(),
        status: "pending".to_string(),
    }))
}

/// GET /api/sessions — List the current user's session history.
pub async fn list_sessions(
    Extension(claims): Extension<Claims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<SessionsResponse>, ApiError> {
    let user_id = claims
        .user_id()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let sessions = sqlx::query_as::<_, SessionRow>(
        r#"
        SELECT
            s.id, s.machine_id, m.name AS machine_name, s.status, s.started_at, s.ended_at,
            s.bytes_sent, s.bytes_received
        FROM sessions s
        JOIN machines m ON m.id = s.machine_id
        WHERE s.user_id = $1
        ORDER BY s.started_at DESC
        LIMIT 100
        "#,
    )
    .bind(user_id)
    .fetch_all(state.db.pool())
    .await?;

    Ok(Json(SessionsResponse {
        sessions: sessions
            .into_iter()
            .map(|session| SessionResponse {
                id: session.id.to_string(),
                machine_id: session.machine_id.to_string(),
                machine_name: session.machine_name,
                status: session.status,
                started_at: session.started_at.to_rfc3339(),
                ended_at: session.ended_at.map(|ended_at| ended_at.to_rfc3339()),
                bytes_sent: session.bytes_sent,
                bytes_received: session.bytes_received,
            })
            .collect(),
    }))
}

async fn ensure_usage_quota(
    state: &AppState,
    machine: &MachineAccessRow,
    user_id: uuid::Uuid,
) -> Result<(), ApiError> {
    let quota = PlanQuota::for_plan(&machine.plan);
    let Some(monthly_minutes) = quota.monthly_minutes else {
        return Ok(());
    };

    let used_seconds: i64 = if let Some(org_id) = machine.organization_id {
        sqlx::query_scalar(
            r#"
            SELECT COALESCE(SUM(EXTRACT(EPOCH FROM (COALESCE(s.ended_at, NOW()) - s.started_at)))::BIGINT, 0)
            FROM sessions s
            JOIN machines m ON m.id = s.machine_id
            WHERE m.organization_id = $1
              AND s.started_at >= date_trunc('month', NOW())
            "#,
        )
        .bind(org_id)
        .fetch_one(state.db.pool())
        .await?
    } else {
        sqlx::query_scalar(
            r#"
            SELECT COALESCE(SUM(EXTRACT(EPOCH FROM (COALESCE(s.ended_at, NOW()) - s.started_at)))::BIGINT, 0)
            FROM sessions s
            JOIN machines m ON m.id = s.machine_id
            WHERE m.user_id = $1
              AND m.organization_id IS NULL
              AND s.started_at >= date_trunc('month', NOW())
            "#,
        )
        .bind(machine.user_id.unwrap_or(user_id))
        .fetch_one(state.db.pool())
        .await?
    };

    let used_minutes = used_seconds.div_euclid(60);
    if used_minutes >= monthly_minutes {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

struct PlanQuota {
    monthly_minutes: Option<i64>,
}

impl PlanQuota {
    fn for_plan(plan: &str) -> Self {
        match plan {
            "team" | "pro" => Self {
                monthly_minutes: Some(50_000),
            },
            "enterprise" => Self {
                monthly_minutes: None,
            },
            _ => Self {
                monthly_minutes: Some(120),
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ConnectResponse {
    pub session_id: String,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct SessionsResponse {
    pub sessions: Vec<SessionResponse>,
}

#[derive(Debug, Serialize)]
pub struct SessionResponse {
    pub id: String,
    pub machine_id: String,
    pub machine_name: String,
    pub status: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub bytes_sent: i64,
    pub bytes_received: i64,
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: uuid::Uuid,
    machine_id: uuid::Uuid,
    machine_name: String,
    status: String,
    started_at: chrono::DateTime<chrono::Utc>,
    ended_at: Option<chrono::DateTime<chrono::Utc>>,
    bytes_sent: i64,
    bytes_received: i64,
}

#[derive(sqlx::FromRow)]
struct MachineAccessRow {
    id: uuid::Uuid,
    organization_id: Option<uuid::Uuid>,
    user_id: Option<uuid::Uuid>,
    plan: String,
}
