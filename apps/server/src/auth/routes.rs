use axum::{
    extract::{Query, State},
    response::Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::auth::workos::{WorkOsClient, WorkOsOrganizationMembership};
use crate::auth::create_token;
use crate::config::Config;
use crate::db::Database;
use crate::error::ApiError;

pub async fn workos_callback(
    State(state): State<Arc<AppState>>,
    Query(params): Query<CallbackQuery>,
) -> Result<Json<AuthResponse>, ApiError> {
    let code = params.code.ok_or_else(|| ApiError::BadRequest(
        "Missing authorization code".to_string()
    ))?;

    // Exchange code with WorkOS
    let profile = state
        .workos
        .authenticate_with_code(&code)
        .await
        .map_err(|e| ApiError::Internal(format!("WorkOS authentication failed: {}", e)))?;

    // Upsert user in database
    let user = upsert_user(&state.db, &profile).await?;

    // Upsert organizations
    for org_membership in &profile.organizations {
        upsert_organization(&state.db, org_membership).await?;
        upsert_membership(&state.db, &user.id, org_membership).await?;
    }

    // Create JWT session
    let token = create_token(
        &user.id.to_string(),
        &profile.id,
        &profile.email,
        &state.config.jwt_secret,
        state.config.jwt_expiry_hours,
    )
    .map_err(|e| ApiError::Internal(format!("Failed to create JWT: {}", e)))?;

    Ok(Json(AuthResponse {
        token,
        user: UserResponse {
            id: user.id.to_string(),
            workos_user_id: profile.id,
            email: profile.email,
            first_name: profile.first_name,
            last_name: profile.last_name,
            avatar_url: profile.profile_picture_url,
            organizations: profile
                .organizations
                .into_iter()
                .map(|o| {
                    let name = o.organization.name;
                    OrganizationResponse {
                        id: o.organization.id.clone(),
                        workos_org_id: o.organization.id,
                        slug: slugify(&name),
                        name,
                        role: o.role.map(|r| r.slug).unwrap_or_else(|| "member".to_string()),
                    }
                })
                .collect(),
        },
    }))
}

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub workos: WorkOsClient,
    pub config: Config,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AuthResponse {
    pub token: String,
    pub user: UserResponse,
}

#[derive(Debug, Serialize)]
pub struct UserResponse {
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

// --- Database helpers ---

async fn upsert_user(
    db: &Database,
    profile: &crate::auth::workos::WorkOsUserProfile,
) -> Result<UserRow, ApiError> {
    let user = sqlx::query_as::<_, UserRow>(
        r#"
        INSERT INTO users (workos_user_id, email, first_name, last_name, avatar_url)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (workos_user_id) DO UPDATE SET
            email = EXCLUDED.email,
            first_name = EXCLUDED.first_name,
            last_name = EXCLUDED.last_name,
            avatar_url = EXCLUDED.avatar_url,
            updated_at = NOW()
        RETURNING id, workos_user_id, email, first_name, last_name, avatar_url, stripe_customer_id, created_at, updated_at
        "#
    )
    .bind(&profile.id)
    .bind(&profile.email)
    .bind(&profile.first_name)
    .bind(&profile.last_name)
    .bind(&profile.profile_picture_url)
    .fetch_one(db.pool())
    .await?;

    Ok(user)
}

async fn upsert_organization(
    db: &Database,
    membership: &WorkOsOrganizationMembership,
) -> Result<(), ApiError> {
    sqlx::query(
        r#"
        INSERT INTO organizations (workos_org_id, name, slug)
        VALUES ($1, $2, $3)
        ON CONFLICT (workos_org_id) DO UPDATE SET
            name = EXCLUDED.name,
            slug = EXCLUDED.slug
        "#
    )
    .bind(&membership.organization.id)
    .bind(&membership.organization.name)
    .bind(slugify(&membership.organization.name))
    .execute(db.pool())
    .await?;

    Ok(())
}

async fn upsert_membership(
    db: &Database,
    user_id: &uuid::Uuid,
    membership: &WorkOsOrganizationMembership,
) -> Result<(), ApiError> {
    let role = membership
        .role
        .as_ref()
        .map(|r| r.slug.as_str())
        .unwrap_or("member");

    sqlx::query(
        r#"
        INSERT INTO organization_members (organization_id, user_id, role)
        SELECT o.id, $1, $2
        FROM organizations o
        WHERE o.workos_org_id = $3
        ON CONFLICT (organization_id, user_id) DO UPDATE SET
            role = EXCLUDED.role
        "#
    )
    .bind(user_id)
    .bind(role)
    .bind(&membership.organization.id)
    .execute(db.pool())
    .await?;

    Ok(())
}

fn slugify(name: &str) -> String {
    name.to_lowercase()
        .replace(' ', "-")
        .replace(|c: char| !c.is_alphanumeric() && c != '-', "")
}

#[derive(sqlx::FromRow)]
struct UserRow {
    id: uuid::Uuid,
    workos_user_id: String,
    email: String,
    first_name: Option<String>,
    last_name: Option<String>,
    avatar_url: Option<String>,
    stripe_customer_id: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}
