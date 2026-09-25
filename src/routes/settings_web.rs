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
use crate::localization::{LocalizationContext, MessageKey};
use crate::models::{
    BusinessLocale, BusinessSettings, UpdateBusinessLocale, UpdateBusinessSettings,
};
use crate::routes::{currency_options, AppState};
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
    currency_options: Vec<crate::routes::CurrencyOption>,
    localization: LocalizationContext,
    nav_key: &'static str,
    nav: Nav,
    saved: bool,
    error: Option<String>,
    preview_message: String,
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
        let persistence_order: BTreeMap<&str, usize> = current_locales
            .iter()
            .enumerate()
            .map(|(index, locale)| (locale.locale_code.as_str(), index))
            .collect();
        let mut locales: Vec<_> = (0..current_locales.len())
            .map(|index| UpdateBusinessLocale {
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
        locales.sort_by_key(|locale| {
            persistence_order
                .get(locale.locale_code.as_str())
                .copied()
                .unwrap_or(usize::MAX)
        });
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
                validation_message(&error, &localization),
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
    let mut current_locales = current_locales;
    if let Some(default_index) = current_locales
        .iter()
        .position(|locale| locale.locale_code == settings.default_locale_code)
    {
        let default_locale = current_locales.remove(default_index);
        current_locales.insert(0, default_locale);
    }

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
    let preview_message = localization.tr_with(
        MessageKey::SettingsPreview,
        &[("preview", preview.as_str())],
    );
    let effective_currency_code = submitted.as_ref().map_or_else(
        || settings.currency_code.clone(),
        |form| form.currency_code.clone(),
    );
    let currency_options = currency_options(&effective_currency_code);
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
            currency_code: effective_currency_code,
            timezone: submitted
                .as_ref()
                .map_or_else(|| settings.timezone.clone(), |form| form.timezone.clone()),
            created_at: settings.created_at,
            updated_at: settings.updated_at,
        },
        locales,
        currency_options,
        localization,
        nav_key: "settings",
        nav: Nav::for_principal(principal),
        saved,
        error,
        preview_message,
    };
    Ok(page)
}

fn validation_message(error: &AppError, localization: &LocalizationContext) -> String {
    let message = error.to_string();
    let message = message
        .strip_prefix("validation error: ")
        .unwrap_or(&message);
    let key = match message {
        "El nombre del negocio debe tener entre 1 y 160 caracteres." => {
            MessageKey::ValidationBusinessNameLength
        }
        "La moneda debe ser un código de tres letras mayúsculas." => {
            MessageKey::ValidationCurrencyCode
        }
        "La zona horaria es obligatoria." => MessageKey::ValidationTimezoneRequired,
        "The submitted locale profiles do not match the configured locales." => {
            MessageKey::ValidationLocaleProfiles
        }
        "El nombre visible del locale debe tener entre 1 y 128 caracteres." => {
            MessageKey::ValidationLocaleDisplayName
        }
        "A locale profile was submitted more than once." => MessageKey::ValidationLocaleDuplicate,
        "The default locale must exist and remain enabled." => {
            MessageKey::ValidationDefaultLocaleEnabled
        }
        _ => return message.to_owned(),
    };
    localization.tr(key).to_owned()
}

fn render<T: Template>(template: T) -> AppResult<String> {
    template
        .render()
        .map_err(|error| AppError::Internal(error.to_string()))
}

pub fn router() -> Router<AppState> {
    Router::new().route("/settings", get(settings_page).post(settings_submit))
}
