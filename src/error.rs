use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::models::PriceRefusal;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("validation error: {0}")]
    Validation(String),

    /// A product price or cost rule refused the input, carrying the RULE rather
    /// than its message.
    ///
    /// It is a variant of its own, and not another `Validation(String)`, for
    /// one reason: a surface that reads a refusal in the operator's language
    /// has to be able to tell WHICH rule refused, and a string cannot be told
    /// apart from every other string without being matched — which is how two
    /// surfaces of one rule drift into two languages.
    ///
    /// This is a PRESENTATION boundary, not a behavior change. `Display` and the
    /// response body are the exact English text the `Validation` variant carried
    /// before, so the JSON API and every other non-localized consumer are
    /// byte-identical; the web surfaces that have a `LocalizationContext` map
    /// the carried rule into the active locale instead.
    ///
    /// WHO CAN PRODUCE IT, and what happens if a surface that is not one of them
    /// ever does. Exactly two places construct this: `InventoryService`'s
    /// `validate_product`, which runs the price and cost rules for every
    /// `create_product`/`update_product` caller, and the product form-shape
    /// gate in `routes/inventory_web.rs`, which turns an emptied manual price
    /// into `SalePriceRequired` before the service is reached. So it exists
    /// exactly where a product price or cost rule runs — the product routes, the
    /// JSON product API, and the purchase record page's "apply line cost" action,
    /// which writes a product's cost through the same service. No tax route
    /// calls `create_product`/`update_product` or the form-shape gate, so this is
    /// unreachable from `routes/settings_web.rs`.
    ///
    /// Were it to arrive there anyway, it would NOT become a 500.
    /// `taxes_refusal_response` returns `Err(error)` for the arm its
    /// `refusal_status` does not recognize, and the `IntoResponse` impl below
    /// gives this variant its own 400 with its English body: the handler would
    /// lose the tax page's localized restyling, never its status class. (An
    /// earlier note in the change's own handoff claimed the unrecognized arm
    /// would report it as a fault; that was wrong, and this paragraph is the
    /// correction, kept because the question is worth answering in the code.)
    #[error("validation error: {0}")]
    PriceRefused(PriceRefusal),

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

    /// The request body exceeds the handler's own size limit (413). The
    /// app-wide JSON shape reaches the operator in Spanish: an oversized form
    /// is a refusal the operator reads, not an English plain-text buffering
    /// error from the extractor layer.
    #[error("payload too large: {0}")]
    PayloadTooLarge(String),

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
            // The same status and the same body the `Validation` variant
            // answered with before the refusal became typed: the English text is
            // part of the wire contract, not a translation choice.
            Self::PriceRefused(r) => (StatusCode::BAD_REQUEST, r.as_str().to_string()),
            Self::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
            Self::Unauthorized(m) => (StatusCode::UNAUTHORIZED, m.clone()),
            Self::Forbidden(m) => (StatusCode::FORBIDDEN, m.clone()),
            Self::PayloadTooLarge(m) => (StatusCode::PAYLOAD_TOO_LARGE, m.clone()),
            Self::Database(e) => {
                tracing::error!(error = %e, "database error");
                // Map unique constraint to 409
                let s = e.to_string();
                if s.contains("UNIQUE constraint failed") {
                    (StatusCode::CONFLICT, "resource already exists".to_string())
                } else {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "database error".to_string(),
                    )
                }
            }
            Self::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m.clone()),
        };

        let body = Json(json!({ "error": msg }));
        (status, body).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;
