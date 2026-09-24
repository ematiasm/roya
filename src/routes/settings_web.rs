use std::collections::BTreeMap;

use askama::Template;
use axum::{
    extract::{Extension, Form, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
    Router,
};
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::localization::LocalizationContext;
use crate::models::{
    BusinessLocale, BusinessSettings, UpdateBusinessLocale, UpdateBusinessSettings,
};
use crate::routes::AppState;
use crate::security::authz::{Nav, Principal, Require, SettingsManage};
use crate::services::settings::UpdateBusinessConfiguration;

struct SettingsLocaleRow {
    code: String,
    display_name: String,
    is_enabled: bool,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    settings: BusinessSettings,
    locales: Vec<SettingsLocaleRow>,
    localization: LocalizationContext,
    nav_key: &'static str,
    nav: Nav,
    saved: bool,
    error: Option<String>,
    preview: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SettingsForm {
    #[serde(default)]
    pub business_name: String,
    #[serde(default)]
    pub default_locale_code: String,
    #[serde(default)]
    pub currency_code: String,
    #[serde(default)]
    pub timezone: String,
    #[serde(flatten)]
    pub locale_fields: BTreeMap<String, String>,
}

impl SettingsForm {
    fn to_update(&self, current_locales: &[BusinessLocale]) -> UpdateBusinessConfiguration {
        let locales = current_locales
            .iter()
            .enumerate()
            .map(|(index, _)| UpdateBusinessLocale {
                locale_code: self
                    .locale_fields
                    .get(&format!("locale_code_{index}"))
                    .cloned()
                    .unwrap_or_default(),
                display_name: self
                    .locale_fields
                    .get(&format!("display_name_{index}"))
                    .cloned()
                    .unwrap_or_default(),
                is_enabled: self.locale_fields.contains_key(&format!("enabled_{index}")),
            })
            .collect();
        UpdateBusinessConfiguration {
            settings: UpdateBusinessSettings {
                business_name: self.business_name.clone(),
                default_locale_code: self.default_locale_code.clone(),
                currency_code: self.currency_code.clone(),
                timezone: self.timezone.clone(),
            },
            locales,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct SettingsPageQuery {
    #[serde(default)]
    saved: bool,
}

async fn settings_page(
    State(state): State<AppState>,
    _: Require<SettingsManage>,
    Extension(localization): Extension<LocalizationContext>,
    principal: Extension<Principal>,
    Query(query): Query<SettingsPageQuery>,
) -> AppResult<Response> {
    let (settings, locales) = state.settings_service.load().await?;
    let page = page_response(
        settings,
        locales,
        None,
        query.saved,
        None,
        localization,
        &principal,
    )?;
    Ok(Html(render(page)?).into_response())
}

async fn settings_submit(
    State(state): State<AppState>,
    _: Require<SettingsManage>,
    Extension(localization): Extension<LocalizationContext>,
    principal: Extension<Principal>,
    Form(form): Form<SettingsForm>,
) -> AppResult<Response> {
    let (_, current_locales) = state.settings_service.load().await?;
    let update = form.to_update(&current_locales);
    match state.settings_service.update(update).await {
        Ok(_) => Ok(Redirect::to("/settings?saved=true").into_response()),
        Err(error @ AppError::Validation(_)) => {
            invalid_page_response(
                &state,
                form,
                validation_message(&error),
                localization,
                &principal,
            )
            .await
        }
        Err(error) => Err(error),
    }
}

async fn invalid_page_response(
    state: &AppState,
    form: SettingsForm,
    error: String,
    localization: LocalizationContext,
    principal: &Principal,
) -> AppResult<Response> {
    let (settings, locales) = state.settings_service.load().await?;
    let page = page_response(
        settings,
        locales,
        Some(form),
        false,
        Some(error),
        localization,
        principal,
    )?;
    Ok((StatusCode::BAD_REQUEST, Html(render(page)?)).into_response())
}

fn page_response(
    settings: BusinessSettings,
    current_locales: Vec<BusinessLocale>,
    submitted: Option<SettingsForm>,
    saved: bool,
    error: Option<String>,
    localization: LocalizationContext,
    principal: &Principal,
) -> AppResult<SettingsPage> {
    let locales = current_locales
        .into_iter()
        .enumerate()
        .map(|(index, locale)| SettingsLocaleRow {
            display_name: submitted
                .as_ref()
                .and_then(|form| form.locale_fields.get(&format!("display_name_{index}")))
                .cloned()
                .unwrap_or(locale.display_name),
            is_enabled: submitted
                .as_ref()
                .map(|form| form.locale_fields.contains_key(&format!("enabled_{index}")))
                .unwrap_or(locale.is_enabled),
            code: locale.locale_code,
        })
        .collect();

    let preview =
        localization.format_currency(rust_decimal::Decimal::from_i128_with_scale(123_450, 2));
    let page = SettingsPage {
        settings: BusinessSettings {
            id: settings.id,
            business_name: submitted.as_ref().map_or_else(
                || settings.business_name.clone(),
                |form| form.business_name.clone(),
            ),
            default_locale_code: submitted.as_ref().map_or_else(
                || settings.default_locale_code.clone(),
                |form| form.default_locale_code.clone(),
            ),
            currency_code: submitted.as_ref().map_or_else(
                || settings.currency_code.clone(),
                |form| form.currency_code.clone(),
            ),
            timezone: submitted
                .as_ref()
                .map_or_else(|| settings.timezone.clone(), |form| form.timezone.clone()),
            created_at: settings.created_at,
            updated_at: settings.updated_at,
        },
        locales,
        localization,
        nav_key: "settings",
        nav: Nav::for_principal(principal),
        saved,
        error,
        preview,
    };
    Ok(page)
}

fn validation_message(error: &AppError) -> String {
    let message = error.to_string();
    message
        .strip_prefix("validation error: ")
        .unwrap_or(&message)
        .to_string()
}

fn render<T: Template>(template: T) -> AppResult<String> {
    template
        .render()
        .map_err(|error| AppError::Internal(error.to_string()))
}

pub fn router() -> Router<AppState> {
    Router::new().route("/settings", get(settings_page).post(settings_submit))
}
