use axum::{
    extract::{Extension, Path, State},
    response::Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::auth::Claims;
use crate::auth::routes::AppState;
use crate::error::ApiError;

/// GET /api/machines — List all machines accessible to the current user.
pub async fn list_machines(
    Extension(claims): Extension<Claims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<MachinesResponse>, ApiError> {
    let user_id = uuid::Uuid::parse_str(&claims.sub)
        .map_err(|_| ApiError::Internal("Invalid user ID".to_string()))?;

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
    let user_id = uuid::Uuid::parse_str(&claims.sub)
        .map_err(|_| ApiError::Internal("Invalid user ID".to_string()))?;

    // Generate a registration token
    let token = format!("rkvm_{}", uuid::Uuid::new_v4().to_string().replace("-", ""));
    let token_hash = hash_token(&token);

    let machine = sqlx::query_as::<_, MachineIdRow>(
        r#"
        INSERT INTO machines (user_id, name, hostname, tailscale_ip, platform, registration_token_hash)
        VALUES ($1, $2, $3, $4, $5, $6)
        RETURNING id
        "#
    )
    .bind(user_id)
    .bind(&req.name)
    .bind(&req.hostname)
    .bind(req.tailscale_ip.as_deref())
    .bind(&req.platform)
    .bind(&token_hash)
    .fetch_one(state.db.pool())
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
    let user_id = uuid::Uuid::parse_str(&claims.sub)
        .map_err(|_| ApiError::Internal("Invalid user ID".to_string()))?;

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
    let user_id = uuid::Uuid::parse_str(&claims.sub)
        .map_err(|_| ApiError::Internal("Invalid user ID".to_string()))?;

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

fn hash_token(token: &str) -> String {
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}
