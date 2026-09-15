use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("validation error: {0}")]
    Validation(String),

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            Self::NotFound(m) => (StatusCode::NOT_FOUND, m.clone()),
            Self::Validation(m) => (StatusCode::BAD_REQUEST, m.clone()),
            Self::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
            Self::Database(e) => {
                tracing::error!(error = %e, "database error");
                // Map unique constraint to 409
                let s = e.to_string();
                if s.contains("UNIQUE constraint failed") {
                    (StatusCode::CONFLICT, "resource already exists".to_string())
                } else {
                    (StatusCode::INTERNAL_SERVER_ERROR, "database error".to_string())
                }
            }
            Self::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m.clone()),
        };

        let body = Json(json!({ "error": msg }));
        (status, body).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;
