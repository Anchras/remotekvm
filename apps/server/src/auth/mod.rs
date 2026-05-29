use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub mod middleware;
pub mod routes;
pub mod workos;

/// JWT claims for our own session tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String, // user UUID
    pub workos_user_id: String,
    pub email: String,
    pub iat: usize,
    pub exp: usize,
}

/// Create a JWT token for a user.
pub fn create_token(
    user_id: &str,
    workos_user_id: &str,
    email: &str,
    secret: &str,
    expiry_hours: i64,
) -> Result<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as usize;
    let exp = now + (expiry_hours as usize) * 3600;

    let claims = Claims {
        sub: user_id.to_string(),
        workos_user_id: workos_user_id.to_string(),
        email: email.to_string(),
        iat: now,
        exp,
    };

    let token = jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
    )?;

    Ok(token)
}

/// Validate a JWT token and return the claims.
pub fn validate_token(token: &str, secret: &str) -> Result<Claims> {
    let validation = jsonwebtoken::Validation::default();
    let token_data = jsonwebtoken::decode::<Claims>(
        token,
        &jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )?;

    Ok(token_data.claims)
}

impl Claims {
    /// Parse the `sub` claim as a UUID.
    pub fn user_id(&self) -> anyhow::Result<uuid::Uuid> {
        uuid::Uuid::parse_str(&self.sub)
            .map_err(|e| anyhow::anyhow!("Invalid user ID in JWT claims: {}", e))
    }
}
