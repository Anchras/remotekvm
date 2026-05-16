use axum::{
    extract::{Request, State},
    middleware::Next,
    response::Response,
};
use std::sync::Arc;

use crate::auth::validate_token;
use crate::error::ApiError;
use crate::state::AppState;

/// JWT authentication middleware.
/// Extracts the Bearer token from the Authorization header, validates it,
/// and adds the user claims to the request extensions.
pub async fn jwt_auth_middleware(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let auth_header = request
        .headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok());

    let token = match auth_header {
        Some(header) if header.starts_with("Bearer ") => &header[7..],
        _ => return Err(ApiError::Unauthorized),
    };

    let claims = validate_token(token, &state.config.jwt_secret)
        .map_err(|_| ApiError::Unauthorized)?;

    // Add claims to request extensions for downstream handlers
    request.extensions_mut().insert(claims);

    Ok(next.run(request).await)
}

// auth_layer removed — jwt_auth_middleware is the canonical entry point
