use axum::{
    extract::{Extension, Path, State},
    response::Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::auth::Claims;
use crate::error::ApiError;
use crate::state::AppState;

pub async fn create_organization(
    Extension(claims): Extension<Claims>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<CreateOrganizationRequest>,
) -> Result<Json<OrganizationResponse>, ApiError> {
    let user_id = claims
        .user_id()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let name = req.name.trim();
    if name.is_empty() {
        return Err(ApiError::BadRequest(
            "organization name is required".to_string(),
        ));
    }

    let slug = req
        .slug
        .as_deref()
        .map(slugify)
        .filter(|slug| !slug.is_empty())
        .unwrap_or_else(|| slugify(name));
    if slug.is_empty() {
        return Err(ApiError::BadRequest(
            "organization slug is required".to_string(),
        ));
    }

    let mut tx = state.db.pool().begin().await?;
    let org = sqlx::query_as::<_, OrganizationRow>(
        r#"
        INSERT INTO organizations (workos_org_id, name, slug)
        VALUES ($1, $2, $3)
        RETURNING id, workos_org_id, name, slug, plan, 'owner'::TEXT AS role
        "#,
    )
    .bind(format!("local_{}", uuid::Uuid::new_v4().simple()))
    .bind(name)
    .bind(&slug)
    .fetch_one(&mut *tx)
    .await
    .map_err(|err| match err {
        sqlx::Error::Database(db_err) if db_err.is_unique_violation() => {
            ApiError::BadRequest("organization slug already exists".to_string())
        }
        other => ApiError::Internal(other.to_string()),
    })?;

    sqlx::query(
        r#"
        INSERT INTO organization_members (organization_id, user_id, role)
        VALUES ($1, $2, 'owner')
        "#,
    )
    .bind(org.id)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(Json(OrganizationResponse {
        id: org.id.to_string(),
        workos_org_id: org.workos_org_id,
        name: org.name,
        slug: org.slug,
        plan: org.plan,
        role: org.role,
    }))
}

pub async fn list_organizations(
    Extension(claims): Extension<Claims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<OrganizationsResponse>, ApiError> {
    let user_id = claims
        .user_id()
        .map_err(|e| ApiError::Internal(e.to_string()))?;

    let organizations = sqlx::query_as::<_, OrganizationRow>(
        r#"
        SELECT o.id, o.workos_org_id, o.name, o.slug, o.plan, om.role
        FROM organizations o
        JOIN organization_members om ON o.id = om.organization_id
        WHERE om.user_id = $1
        ORDER BY o.name ASC
        "#,
    )
    .bind(user_id)
    .fetch_all(state.db.pool())
    .await?;

    Ok(Json(OrganizationsResponse {
        organizations: organizations
            .into_iter()
            .map(|org| OrganizationResponse {
                id: org.id.to_string(),
                workos_org_id: org.workos_org_id,
                name: org.name,
                slug: org.slug,
                plan: org.plan,
                role: org.role,
            })
            .collect(),
    }))
}

pub async fn list_members(
    Extension(claims): Extension<Claims>,
    Path(org_id): Path<uuid::Uuid>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<MembersResponse>, ApiError> {
    let user_id = claims
        .user_id()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    require_org_admin(&state, org_id, user_id).await?;

    let members = sqlx::query_as::<_, MemberRow>(
        r#"
        SELECT u.id, u.email, u.first_name, u.last_name, om.role, om.joined_at
        FROM organization_members om
        JOIN users u ON u.id = om.user_id
        WHERE om.organization_id = $1
        ORDER BY om.role ASC, u.email ASC
        "#,
    )
    .bind(org_id)
    .fetch_all(state.db.pool())
    .await?;

    let pending_invites = sqlx::query_as::<_, InviteRow>(
        r#"
        SELECT oi.id, oi.email, oi.role, oi.created_at, u.email AS invited_by_email
        FROM organization_invites oi
        LEFT JOIN users u ON u.id = oi.invited_by
        WHERE oi.organization_id = $1
          AND oi.accepted_at IS NULL
        ORDER BY oi.created_at ASC
        "#,
    )
    .bind(org_id)
    .fetch_all(state.db.pool())
    .await?;

    Ok(Json(MembersResponse {
        members: members
            .into_iter()
            .map(|member| MemberResponse {
                id: member.id.to_string(),
                email: member.email,
                first_name: member.first_name,
                last_name: member.last_name,
                role: member.role,
                joined_at: member.joined_at.to_rfc3339(),
            })
            .collect(),
        pending_invites: pending_invites
            .into_iter()
            .map(|invite| InviteResponse {
                id: invite.id.to_string(),
                email: invite.email,
                role: invite.role,
                invited_by_email: invite.invited_by_email,
                created_at: invite.created_at.to_rfc3339(),
            })
            .collect(),
    }))
}

pub async fn invite_member(
    Extension(claims): Extension<Claims>,
    Path(org_id): Path<uuid::Uuid>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<InviteMemberRequest>,
) -> Result<Json<InviteMemberResponse>, ApiError> {
    let user_id = claims
        .user_id()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let actor_role = require_org_admin(&state, org_id, user_id).await?;

    let email = req.email.trim().to_lowercase();
    if !email.contains('@') {
        return Err(ApiError::BadRequest("invalid email".to_string()));
    }
    let role = req.role.unwrap_or_else(|| "member".to_string());
    validate_role(&role)?;
    ensure_can_grant_role(&actor_role, &role)?;

    let existing_user =
        sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM users WHERE email = $1")
            .bind(&email)
            .fetch_optional(state.db.pool())
            .await?;

    if let Some(invited_user_id) = existing_user {
        let existing_role = sqlx::query_scalar::<_, String>(
            "SELECT role FROM organization_members WHERE organization_id = $1 AND user_id = $2",
        )
        .bind(org_id)
        .bind(invited_user_id)
        .fetch_optional(state.db.pool())
        .await?;

        if actor_role != "owner" && existing_role.as_deref() == Some("owner") {
            return Err(ApiError::Forbidden);
        }

        sqlx::query(
            r#"
            INSERT INTO organization_members (organization_id, user_id, role)
            VALUES ($1, $2, $3)
            ON CONFLICT (organization_id, user_id) DO UPDATE SET role = EXCLUDED.role
            "#,
        )
        .bind(org_id)
        .bind(invited_user_id)
        .bind(&role)
        .execute(state.db.pool())
        .await?;

        sqlx::query(
            r#"
            UPDATE organization_invites
            SET accepted_at = NOW()
            WHERE organization_id = $1
              AND email = $2
              AND accepted_at IS NULL
            "#,
        )
        .bind(org_id)
        .bind(&email)
        .execute(state.db.pool())
        .await?;

        return Ok(Json(InviteMemberResponse {
            status: "joined".to_string(),
            email,
            role,
        }));
    }

    sqlx::query(
        r#"
        INSERT INTO organization_invites (organization_id, email, role, invited_by)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (organization_id, email) DO UPDATE SET
            role = EXCLUDED.role,
            invited_by = EXCLUDED.invited_by,
            accepted_at = NULL,
            created_at = NOW()
        "#,
    )
    .bind(org_id)
    .bind(&email)
    .bind(&role)
    .bind(user_id)
    .execute(state.db.pool())
    .await?;

    Ok(Json(InviteMemberResponse {
        status: "invited".to_string(),
        email,
        role,
    }))
}

pub async fn share_machine(
    Extension(claims): Extension<Claims>,
    Path((org_id, machine_id)): Path<(uuid::Uuid, uuid::Uuid)>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<ShareMachineResponse>, ApiError> {
    let user_id = claims
        .user_id()
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    require_org_admin(&state, org_id, user_id).await?;

    let result = sqlx::query(
        r#"
        UPDATE machines
        SET organization_id = $1
        WHERE id = $2
          AND (user_id = $3 OR organization_id = $1)
        "#,
    )
    .bind(org_id)
    .bind(machine_id)
    .bind(user_id)
    .execute(state.db.pool())
    .await?;

    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound(
            "machine not found or not shareable".to_string(),
        ));
    }

    Ok(Json(ShareMachineResponse {
        machine_id: machine_id.to_string(),
        organization_id: org_id.to_string(),
        status: "shared".to_string(),
    }))
}

async fn require_org_admin(
    state: &AppState,
    org_id: uuid::Uuid,
    user_id: uuid::Uuid,
) -> Result<String, ApiError> {
    let role = sqlx::query_scalar::<_, String>(
        "SELECT role FROM organization_members WHERE organization_id = $1 AND user_id = $2",
    )
    .bind(org_id)
    .bind(user_id)
    .fetch_optional(state.db.pool())
    .await?;

    match role.as_deref() {
        Some("owner") | Some("admin") => Ok(role.unwrap()),
        Some(_) => Err(ApiError::Forbidden),
        None => Err(ApiError::NotFound("organization not found".to_string())),
    }
}

fn validate_role(role: &str) -> Result<(), ApiError> {
    match role {
        "owner" | "admin" | "member" => Ok(()),
        _ => Err(ApiError::BadRequest("invalid role".to_string())),
    }
}

fn ensure_can_grant_role(actor_role: &str, target_role: &str) -> Result<(), ApiError> {
    match (actor_role, target_role) {
        ("owner", _) => Ok(()),
        ("admin", "admin" | "member") => Ok(()),
        ("admin", "owner") => Err(ApiError::Forbidden),
        _ => Err(ApiError::Forbidden),
    }
}

fn slugify(name: &str) -> String {
    let slug = name
        .trim()
        .to_lowercase()
        .replace(char::is_whitespace, "-")
        .replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "");
    slug.split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[derive(Debug, Deserialize)]
pub struct CreateOrganizationRequest {
    pub name: String,
    pub slug: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct OrganizationsResponse {
    pub organizations: Vec<OrganizationResponse>,
}

#[derive(Debug, Serialize)]
pub struct OrganizationResponse {
    pub id: String,
    pub workos_org_id: String,
    pub name: String,
    pub slug: String,
    pub plan: String,
    pub role: String,
}

#[derive(Debug, Serialize)]
pub struct MembersResponse {
    pub members: Vec<MemberResponse>,
    pub pending_invites: Vec<InviteResponse>,
}

#[derive(Debug, Serialize)]
pub struct MemberResponse {
    pub id: String,
    pub email: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub role: String,
    pub joined_at: String,
}

#[derive(Debug, Serialize)]
pub struct InviteResponse {
    pub id: String,
    pub email: String,
    pub role: String,
    pub invited_by_email: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct InviteMemberRequest {
    pub email: String,
    pub role: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct InviteMemberResponse {
    pub status: String,
    pub email: String,
    pub role: String,
}

#[derive(Debug, Serialize)]
pub struct ShareMachineResponse {
    pub machine_id: String,
    pub organization_id: String,
    pub status: String,
}

#[derive(sqlx::FromRow)]
struct OrganizationRow {
    id: uuid::Uuid,
    workos_org_id: String,
    name: String,
    slug: String,
    plan: String,
    role: String,
}

#[derive(sqlx::FromRow)]
struct MemberRow {
    id: uuid::Uuid,
    email: String,
    first_name: Option<String>,
    last_name: Option<String>,
    role: String,
    joined_at: chrono::DateTime<chrono::Utc>,
}

#[derive(sqlx::FromRow)]
struct InviteRow {
    id: uuid::Uuid,
    email: String,
    role: String,
    created_at: chrono::DateTime<chrono::Utc>,
    invited_by_email: Option<String>,
}
