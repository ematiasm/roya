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

    /// Identity authentication refused (401). The login flow always carries a
    /// fixed generic message so unknown user, wrong password and inactive user
    /// are indistinguishable by status, body or wording.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// Identity authorization refused (403): the principal is authenticated
    /// but lacks the permission the handler declares. Constructed ONLY by the
    /// `Require<P>` extractor (security/authz.rs), whose refusal shapes the
    /// response per caller — JSON for `/api/*` and HTMX, the `forbidden.html`
    /// page for a full-page navigation.
    #[error("forbidden: {0}")]
    Forbidden(String),

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
            Self::Unauthorized(m) => (StatusCode::UNAUTHORIZED, m.clone()),
            Self::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
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
