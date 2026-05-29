use axum::{
    extract::{Query, State},
    response::{IntoResponse, Json, Redirect, Response},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::auth::create_token;
use crate::auth::workos::WorkOsOrganizationMembership;
use crate::db::Database;
use crate::error::ApiError;
use crate::state::AppState;

/// `GET /auth/login?redirect_uri=<loopback>` — begin the AuthKit login.
///
/// Native clients bind a loopback HTTP server and pass its URL as
/// `redirect_uri`. We carry that through the OAuth `state` parameter and
/// redirect the browser to the WorkOS AuthKit authorization endpoint. After the
/// user authenticates, WorkOS calls our `/auth/workos/callback`, which mints a
/// JWT and redirects the browser back to the loopback `redirect_uri`.
pub async fn login_init(
    State(state): State<Arc<AppState>>,
    Query(params): Query<LoginQuery>,
) -> Result<Redirect, ApiError> {
    let redirect_uri = params
        .redirect_uri
        .ok_or_else(|| ApiError::BadRequest("Missing redirect_uri".to_string()))?;

    // Only ever hand a freshly-minted token back to a loopback address. This
    // prevents the login endpoint from being abused as an open redirector that
    // leaks session tokens to an attacker-controlled host.
    if !is_loopback_redirect(&redirect_uri) {
        return Err(ApiError::BadRequest(
            "redirect_uri must be a loopback address".to_string(),
        ));
    }

    // Sign the redirect target into the OAuth `state` (short-lived JWT). The
    // callback only trusts a `state` we minted, which prevents a forged
    // callback from steering the token to an attacker-chosen (even loopback)
    // port without the user having started login here.
    let signed_state = encode_state(&redirect_uri, &state.config.jwt_secret)
        .map_err(|e| ApiError::Internal(format!("failed to sign state: {e}")))?;

    let server_callback = format!("{}/auth/workos/callback", state.config.public_base_url);
    let authorize_url = format!(
        "{base}/user_management/authorize?response_type=code&provider=authkit&client_id={client}&redirect_uri={redirect}&state={st}",
        base = state.config.workos_api_base,
        client = percent_encode(&state.config.workos_client_id),
        redirect = percent_encode(&server_callback),
        st = percent_encode(&signed_state),
    );

    Ok(Redirect::temporary(&authorize_url))
}

/// `GET /auth/workos/callback` — WorkOS calls this with an authorization `code`.
///
/// If a `state` (the original loopback `redirect_uri`) is present, redirect the
/// browser back to it with the freshly-minted token. Otherwise return the
/// session JSON directly (useful for non-browser / API callers).
pub async fn workos_callback(
    State(state): State<Arc<AppState>>,
    Query(params): Query<CallbackQuery>,
) -> Result<Response, ApiError> {
    let code = params
        .code
        .ok_or_else(|| ApiError::BadRequest("Missing authorization code".to_string()))?;

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

    // If the native loopback flow carried a (signed) redirect target, send the
    // token there. We only honor a `state` we issued in `login_init`.
    if let Some(state_param) = params.state {
        let redirect_uri = decode_state(&state_param, &state.config.jwt_secret)
            .map_err(|_| ApiError::BadRequest("invalid or expired state".to_string()))?;
        // Defense in depth: re-validate the decoded target is still loopback.
        if !is_loopback_redirect(&redirect_uri) {
            return Err(ApiError::BadRequest(
                "state redirect_uri must be a loopback address".to_string(),
            ));
        }
        let sep = if redirect_uri.contains('?') { '&' } else { '?' };
        let location = format!("{redirect_uri}{sep}token={}", percent_encode(&token));
        return Ok(Redirect::temporary(&location).into_response());
    }

    let body = AuthResponse {
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
                        role: o
                            .role
                            .map(|r| r.slug)
                            .unwrap_or_else(|| "member".to_string()),
                    }
                })
                .collect(),
        },
    };
    Ok(Json(body).into_response())
}

#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    pub redirect_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    /// Carries the native client's loopback `redirect_uri` through the OAuth flow.
    pub state: Option<String>,
}

/// Whether `uri` is an `http` URL pointing at the loopback interface.
///
/// We accept only literal loopback IPs — not `localhost`, which is resolved by
/// the user's OS resolver and could be shadowed by adversarial DNS, defeating
/// the open-redirect guard.
fn is_loopback_redirect(uri: &str) -> bool {
    let rest = match uri.strip_prefix("http://") {
        Some(r) => r,
        None => return false,
    };
    // Authority is everything up to the first '/', '?', or '#'. Any userinfo
    // (`user@host`) makes the authority not match a bare loopback literal, so
    // such tricks are rejected rather than parsed.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(authority);
    matches!(host, "127.0.0.1" | "[::1]")
}

/// Claims for the short-lived signed OAuth `state` token.
#[derive(Debug, Serialize, Deserialize)]
struct StateClaims {
    redirect_uri: String,
    exp: usize,
}

/// How long a login `state` token stays valid.
const STATE_TTL_SECS: u64 = 600;

/// Sign the loopback `redirect_uri` into a short-lived JWT used as OAuth `state`.
fn encode_state(redirect_uri: &str, secret: &str) -> anyhow::Result<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let claims = StateClaims {
        redirect_uri: redirect_uri.to_string(),
        exp: (now + STATE_TTL_SECS) as usize,
    };
    Ok(jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )?)
}

/// Verify a `state` token (signature + expiry) and recover the `redirect_uri`.
fn decode_state(state: &str, secret: &str) -> anyhow::Result<String> {
    let data = jsonwebtoken::decode::<StateClaims>(
        state,
        &jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
        &jsonwebtoken::Validation::default(),
    )?;
    Ok(data.claims.redirect_uri)
}

/// Minimal RFC 3986 percent-encoding for query-parameter values.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
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
        RETURNING id
        "#,
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
        "#,
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
        "#,
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
}
