use axum::{
    extract::{Extension, State},
    response::Json,
};
use serde::Serialize;
use std::sync::Arc;

use crate::auth::Claims;
use crate::error::ApiError;
use crate::state::AppState;

/// GET /api/me — Returns the current authenticated user.
pub async fn me_handler(
    Extension(claims): Extension<Claims>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<MeResponse>, ApiError> {
    let user = sqlx::query_as::<_, UserRow>(
        r#"
        SELECT id, workos_user_id, email, first_name, last_name, avatar_url
        FROM users
        WHERE id = $1
        "#,
    )
    .bind(
        uuid::Uuid::parse_str(&claims.sub)
            .map_err(|_| ApiError::Internal("Invalid user ID".to_string()))?,
    )
    .fetch_one(state.db.pool())
    .await?;

    let organizations = sqlx::query_as::<_, OrgRow>(
        r#"
        SELECT o.id, o.workos_org_id, o.name, o.slug, om.role
        FROM organizations o
        JOIN organization_members om ON o.id = om.organization_id
        WHERE om.user_id = $1
        "#,
    )
    .bind(user.id)
    .fetch_all(state.db.pool())
    .await?;

    Ok(Json(MeResponse {
        id: user.id.to_string(),
        workos_user_id: user.workos_user_id,
        email: user.email,
        first_name: user.first_name,
        last_name: user.last_name,
        avatar_url: user.avatar_url,
        organizations: organizations
            .into_iter()
            .map(|o| OrganizationResponse {
                id: o.id.to_string(),
                workos_org_id: o.workos_org_id,
                name: o.name,
                slug: o.slug,
                role: o.role,
            })
            .collect(),
    }))
}

#[derive(Debug, Serialize)]
pub struct MeResponse {
    pub id: String,
    pub workos_user_id: String,
    pub email: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub avatar_url: Option<String>,
    pub organizations: Vec<OrganizationResponse>,
}

#[derive(Debug, Serialize)]
pub struct OrganizationResponse {
    pub id: String,
    pub workos_org_id: String,
    pub name: String,
    pub slug: String,
    pub role: String,
}

#[derive(sqlx::FromRow)]
struct UserRow {
    id: uuid::Uuid,
    workos_user_id: String,
    email: String,
    first_name: Option<String>,
    last_name: Option<String>,
    avatar_url: Option<String>,
}

#[derive(sqlx::FromRow)]
struct OrgRow {
    id: uuid::Uuid,
    workos_org_id: String,
    name: String,
    slug: String,
    role: String,
}
