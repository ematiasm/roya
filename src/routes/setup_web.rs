use askama::Template;
use axum::{
    extract::{Form, Request, State},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::localization::{LocalizationContext, MessageKey};
use crate::routes::{currency_options, AppState};
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

struct SetupLocale {
    code: &'static str,
    display_name: &'static str,
}

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupPage {
    locales: Vec<SetupLocale>,
    currency_options: Vec<crate::routes::CurrencyOption>,
    localization: LocalizationContext,
}

pub async fn setup_page(State(state): State<AppState>) -> AppResult<Response> {
    state.refresh_setup_requirement().await?;
    if !state.setup_required() {
        return Ok(Redirect::to("/login").into_response());
    }

    // No business configuration exists yet, so the selected form locale is not
    // an effective language yet. The GET bootstrap deliberately uses the
    // existing fallback until a configuration is persisted; POST values remain
    // canonical and are never used to infer the presentation language.
    let localization = LocalizationContext::fallback();
    let locales = LOCALE_CATALOG
        .iter()
        .map(|locale| SetupLocale {
            code: locale.code,
            display_name: match locale.code {
                "es-AR" => localization.tr(MessageKey::SetupLocaleEsAr),
                "es-ES" => localization.tr(MessageKey::SetupLocaleEsEs),
                "en-US" => localization.tr(MessageKey::SetupLocaleEnUs),
                _ => locale.display_name,
            },
        })
        .collect();
    let html = SetupPage {
        locales,
        currency_options: currency_options("ARS"),
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
