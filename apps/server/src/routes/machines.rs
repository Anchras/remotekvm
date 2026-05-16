use axum::{
    extract::{Extension, Path, State},
    response::Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::auth::Claims;
use crate::error::ApiError;
use crate::state::AppState;

/// GET /api/machines — List all machines accessible to the current user.
pub async fn list_machines(
    Extension(claims): Extension<Claims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<MachinesResponse>, ApiError> {
    let user_id = claims.user_id().map_err(|e| ApiError::Internal(e.to_string()))?;

    // Fetch machines owned by user or in their organizations
    let machines = sqlx::query_as::<_, MachineRow>(
        r#"
        SELECT 
            m.id, m.name, m.hostname, m.tailscale_ip, m.platform, 
            m.online, m.last_seen, m.created_at,
            u.id as owner_id, u.email as owner_email,
            o.id as org_id, o.name as org_name
        FROM machines m
        LEFT JOIN users u ON m.user_id = u.id
        LEFT JOIN organizations o ON m.organization_id = o.id
        WHERE m.user_id = $1
           OR m.organization_id IN (
               SELECT organization_id FROM organization_members WHERE user_id = $1
           )
        ORDER BY m.online DESC, m.name ASC
        "#
    )
    .bind(user_id)
    .fetch_all(state.db.pool())
    .await?;

    Ok(Json(MachinesResponse {
        machines: machines.into_iter().map(|m| MachineResponse {
            id: m.id.to_string(),
            name: m.name,
            hostname: m.hostname,
            tailscale_ip: m.tailscale_ip.map(|ip| ip.to_string()),
            platform: m.platform,
            online: m.online,
            last_seen: m.last_seen.map(|t| t.to_rfc3339()),
            created_at: m.created_at.to_rfc3339(),
            owner: m.owner_id.map(|id| UserRef {
                id: id.to_string(),
                email: m.owner_email.unwrap_or_default(),
            }),
            organization: m.org_id.map(|id| OrgRef {
                id: id.to_string(),
                name: m.org_name.unwrap_or_default(),
            }),
        }).collect(),
    }))
}

/// POST /api/machines — Register a new machine.
pub async fn register_machine(
    Extension(claims): Extension<Claims>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<RegisterMachineRequest>,
) -> Result<Json<RegisterMachineResponse>, ApiError> {
    let user_id = claims.user_id().map_err(|e| ApiError::Internal(e.to_string()))?;

    // Validate organization_id if provided
    if let Some(org_id) = req.organization_id {
        let is_member = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM organization_members WHERE organization_id = $1 AND user_id = $2)"
        )
        .bind(org_id)
        .bind(user_id)
        .fetch_one(state.db.pool())
        .await?;

        if !is_member {
            return Err(ApiError::Forbidden);
        }
    }

    // Generate a registration token
    let token = format!("rkvm_{}", uuid::Uuid::new_v4().to_string().replace("-", ""));
    let token_hash = crate::util::hash_token(&token);

    let machine = sqlx::query_as::<_, MachineIdRow>(
        r#"
        INSERT INTO machines (user_id, organization_id, name, hostname, tailscale_ip, platform, registration_token_hash)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING id
        "#
    )
    .bind(user_id)
    .bind(req.organization_id)
    .bind(&req.name)
    .bind(&req.hostname)
    .bind(req.tailscale_ip.as_deref())
    .bind(&req.platform)
    .bind(&token_hash)
    .fetch_one(state.db.pool())
    .await?;

    // Store token in rotation history
    sqlx::query(
        "INSERT INTO machine_tokens (machine_id, token_hash) VALUES ($1, $2)"
    )
    .bind(machine.id)
    .bind(&token_hash)
    .execute(state.db.pool())
    .await?;

    Ok(Json(RegisterMachineResponse {
        id: machine.id.to_string(),
        registration_token: token,
    }))
}

/// POST /api/machines/{id}/rotate-token — Rotate the registration token.
pub async fn rotate_token(
    Extension(claims): Extension<Claims>,
    Path(machine_id): Path<uuid::Uuid>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<RegisterMachineResponse>, ApiError> {
    let user_id = claims.user_id().map_err(|e| ApiError::Internal(e.to_string()))?;

    // Verify ownership
    let machine = sqlx::query_as::<_, MachineIdRow>(
        "SELECT id FROM machines WHERE id = $1 AND user_id = $2"
    )
    .bind(machine_id)
    .bind(user_id)
    .fetch_optional(state.db.pool())
    .await?
    .ok_or_else(|| ApiError::NotFound("Machine not found".to_string()))?;

    // Revoke old tokens
    sqlx::query("UPDATE machine_tokens SET revoked_at = NOW() WHERE machine_id = $1 AND revoked_at IS NULL")
        .bind(machine.id)
        .execute(state.db.pool())
        .await?;

    // Generate new token
    let token = format!("rkvm_{}", uuid::Uuid::new_v4().to_string().replace("-", ""));
    let token_hash = crate::util::hash_token(&token);

    // Update machine with new token
    sqlx::query("UPDATE machines SET registration_token_hash = $1 WHERE id = $2")
        .bind(&token_hash)
        .bind(machine.id)
        .execute(state.db.pool())
        .await?;

    // Store new token in rotation history
    sqlx::query("INSERT INTO machine_tokens (machine_id, token_hash) VALUES ($1, $2)")
        .bind(machine.id)
        .bind(&token_hash)
        .execute(state.db.pool())
        .await?;

    Ok(Json(RegisterMachineResponse {
        id: machine.id.to_string(),
        registration_token: token,
    }))
}

/// GET /api/machines/{id} — Get a specific machine.
pub async fn get_machine(
    Extension(claims): Extension<Claims>,
    Path(machine_id): Path<uuid::Uuid>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<MachineResponse>, ApiError> {
    let user_id = claims.user_id().map_err(|e| ApiError::Internal(e.to_string()))?;

    let machine = sqlx::query_as::<_, MachineRow>(
        r#"
        SELECT 
            m.id, m.name, m.hostname, m.tailscale_ip, m.platform, 
            m.online, m.last_seen, m.created_at,
            u.id as owner_id, u.email as owner_email,
            o.id as org_id, o.name as org_name
        FROM machines m
        LEFT JOIN users u ON m.user_id = u.id
        LEFT JOIN organizations o ON m.organization_id = o.id
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

    Ok(Json(MachineResponse {
        id: machine.id.to_string(),
        name: machine.name,
        hostname: machine.hostname,
        tailscale_ip: machine.tailscale_ip.map(|ip| ip.to_string()),
        platform: machine.platform,
        online: machine.online,
        last_seen: machine.last_seen.map(|t| t.to_rfc3339()),
        created_at: machine.created_at.to_rfc3339(),
        owner: machine.owner_id.map(|id| UserRef {
            id: id.to_string(),
            email: machine.owner_email.unwrap_or_default(),
        }),
        organization: machine.org_id.map(|id| OrgRef {
            id: id.to_string(),
            name: machine.org_name.unwrap_or_default(),
        }),
    }))
}

/// DELETE /api/machines/{id} — Delete a machine.
pub async fn delete_machine(
    Extension(claims): Extension<Claims>,
    Path(machine_id): Path<uuid::Uuid>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user_id = claims.user_id().map_err(|e| ApiError::Internal(e.to_string()))?;

    let result = sqlx::query(
        r#"
        DELETE FROM machines
        WHERE id = $1 AND user_id = $2
        "#
    )
    .bind(machine_id)
    .bind(user_id)
    .execute(state.db.pool())
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound("Machine not found or not owned by you".to_string()));
    }

    Ok(Json(serde_json::json!({"status": "deleted"})))
}

// --- Request/Response types ---

#[derive(Debug, Deserialize)]
pub struct RegisterMachineRequest {
    pub name: String,
    pub hostname: String,
    pub tailscale_ip: Option<String>,
    pub platform: String,
    pub organization_id: Option<uuid::Uuid>,
}

#[derive(Debug, Serialize)]
pub struct RegisterMachineResponse {
    pub id: String,
    pub registration_token: String,
}

#[derive(Debug, Serialize)]
pub struct MachinesResponse {
    pub machines: Vec<MachineResponse>,
}

#[derive(Debug, Serialize)]
pub struct MachineResponse {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub tailscale_ip: Option<String>,
    pub platform: String,
    pub online: bool,
    pub last_seen: Option<String>,
    pub created_at: String,
    pub owner: Option<UserRef>,
    pub organization: Option<OrgRef>,
}

#[derive(Debug, Serialize)]
pub struct UserRef {
    pub id: String,
    pub email: String,
}

#[derive(Debug, Serialize)]
pub struct OrgRef {
    pub id: String,
    pub name: String,
}

// --- Database rows ---

#[derive(sqlx::FromRow)]
struct MachineRow {
    id: uuid::Uuid,
    name: String,
    hostname: String,
    tailscale_ip: Option<String>,
    platform: String,
    online: bool,
    last_seen: Option<chrono::DateTime<chrono::Utc>>,
    created_at: chrono::DateTime<chrono::Utc>,
    owner_id: Option<uuid::Uuid>,
    owner_email: Option<String>,
    org_id: Option<uuid::Uuid>,
    org_name: Option<String>,
}

#[derive(sqlx::FromRow)]
struct MachineIdRow {
    id: uuid::Uuid,
}

// --- Helpers ---

// hash_token moved to crate::util
