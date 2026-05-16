use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    Forbidden,
    NotFound(String),
    BadRequest(String),
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, error_message) = match &self {
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "Unauthorized"),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "Forbidden"),
            ApiError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.as_str()),
            ApiError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.as_str()),
            ApiError::Internal(msg) => {
                tracing::error!("internal error: {}", msg);
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
            }
        };

        let code = match self {
            ApiError::Unauthorized => "UNAUTHORIZED",
            ApiError::Forbidden => "FORBIDDEN",
            ApiError::NotFound(_) => "NOT_FOUND",
            ApiError::BadRequest(_) => "BAD_REQUEST",
            ApiError::Internal(_) => "INTERNAL_ERROR",
        };

        let body = Json(json!({
            "error": {
                "code": code,
                "message": error_message
            }
        }));

        (status, body).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        match err {
            sqlx::Error::RowNotFound => ApiError::NotFound("Resource not found".to_string()),
            _ => ApiError::Internal(err.to_string()),
        }
    }
}
