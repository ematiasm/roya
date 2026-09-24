use askama::Template;
use axum::{
    extract::{Extension, Form, Request, State},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::localization::LocalizationContext;
use crate::routes::AppState;
use crate::services::setup::{SetupInput, LOCALE_CATALOG};

#[derive(Debug, Deserialize, Default)]
pub struct SetupForm {
    #[serde(default)]
    pub business_name: String,
    #[serde(default)]
    pub default_locale_code: String,
    #[serde(default)]
    pub currency_code: String,
    #[serde(default)]
    pub timezone: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub password: String,
}

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupPage {
    locales: &'static [crate::services::setup::LocaleDefinition],
    localization: LocalizationContext,
}

pub async fn setup_page(
    State(state): State<AppState>,
    Extension(localization): Extension<LocalizationContext>,
) -> AppResult<Response> {
    state.refresh_setup_requirement().await?;
    if !state.setup_required() {
        return Ok(Redirect::to("/login").into_response());
    }

    let html = SetupPage {
        locales: LOCALE_CATALOG,
        localization,
    }
    .render()
    .map_err(|error| AppError::Internal(error.to_string()))?;
    Ok(Html(html).into_response())
}

pub async fn setup_submit(
    State(state): State<AppState>,
    Form(form): Form<SetupForm>,
) -> AppResult<Response> {
    state.refresh_setup_requirement().await?;
    if !state.setup_required() {
        return Err(AppError::Conflict(
            "La configuración inicial ya fue completada.".into(),
        ));
    }

    state
        .setup_service
        .create(&SetupInput {
            business_name: form.business_name,
            default_locale_code: form.default_locale_code,
            currency_code: form.currency_code,
            timezone: form.timezone,
            username: form.username,
            display_name: form.display_name,
            password: form.password,
        })
        .await?;
    state.mark_setup_complete();
    Ok(Redirect::to("/login").into_response())
}

pub async fn setup_gate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if !state.setup_required()
        || path == "/setup"
        || path == "/static"
        || path.starts_with("/static/")
    {
        return next.run(request).await;
    }

    if path.starts_with("/api/") {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "application setup required" })),
        )
            .into_response();
    }

    Redirect::temporary("/setup").into_response()
}

pub fn router() -> Router<AppState> {
    Router::new().route("/setup", get(setup_page).post(setup_submit))
}
