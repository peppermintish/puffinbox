use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict(String),
    Unavailable,
    RateLimited,
    Internal(String),
}

#[derive(Serialize)]
struct ErrorBody {
    #[serde(rename = "Message")]
    message: String,
    #[serde(rename = "ErrorCode")]
    error_code: u16,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "Authentication required".to_owned(),
            ),
            Self::Forbidden => (StatusCode::FORBIDDEN, "Access denied".to_owned()),
            Self::NotFound => (StatusCode::NOT_FOUND, "Resource not found".to_owned()),
            Self::Conflict(message) => (StatusCode::CONFLICT, message),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                "Service temporarily unavailable".to_owned(),
            ),
            Self::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "Too many requests".to_owned(),
            ),
            // Internal detail is kept in structured logs and is never returned to clients.
            Self::Internal(message) => {
                tracing::error!(error = %message, "request failed internally");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal server error".to_owned(),
                )
            }
        };
        let body = ErrorBody {
            message,
            error_code: status.as_u16(),
        };
        (status, Json(body)).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "database request failed");
        Self::Internal("database operation failed".to_owned())
    }
}
