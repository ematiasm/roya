use std::collections::BTreeMap;

use askama::Template;
use axum::{
    extract::{Extension, Form, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

use crate::error::{AppError, AppResult};
use crate::localization::{LocalizationContext, MessageKey};
use crate::models::{
    BusinessLocale, BusinessSettings, NewTax, Tax, UpdateBusinessLocale, UpdateBusinessSettings,
    UpdateTax,
};
use crate::routes::{currency_options, AppState};
use crate::security::authz::{Nav, Principal, Require, SettingsManage};
use crate::services::settings::UpdateBusinessConfiguration;
use crate::services::taxes::{
    TAX_DELETE_BLOCKED, TAX_DELETE_BLOCKED_BY_HISTORY, TAX_DELETE_BLOCKED_BY_PRODUCTS,
};

struct SettingsLocaleRow {
    code: String,
    display_name: String,
    is_enabled: bool,
}

/// Which section of `/settings` the request selected.
///
/// The smallest mechanism that fits a single-form page: one query parameter on
/// the same route, with the business tab as the default. `/settings` keeps its
/// URL, its submission and its markup exactly as before, and a tab that renders
/// nothing the operator asked for costs one extra navigation rather than a
/// second page, a second route and a second set of permission checks.
///
/// An UNKNOWN value is not an error and not a new tab: it falls back to the
/// business tab, the same lenient rule the catalogue filter already follows,
/// so a stale bookmark lands somewhere real.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    Business,
    Taxes,
}

impl SettingsTab {
    fn from_query(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("taxes") => Self::Taxes,
            _ => Self::Business,
        }
    }
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
    /// The selected section. Only the taxes tab reads the catalogue, so the
    /// business tab's request never touches a tax table.
    tab: SettingsTab,
    /// The catalogue rows, in catalogue order. Empty on the business tab, which
    /// is where the "keep the list empty" default does the honest work.
    ///
    /// Named for the included partial's field, not for the page: an Askama
    /// `include` renders against the INCLUDING struct, so the page and
    /// `SettingsTaxesPartial` have to agree on the name the rows are read by.
    rows: Vec<SettingsTaxRow>,
}

impl SettingsPage {
    /// The section the page is showing. A method rather than a field
    /// comparison because the template already has a `settings` field, and a
    /// `settings::…` path in a template would resolve to that field.
    fn is_taxes_tab(&self) -> bool {
        self.tab == SettingsTab::Taxes
    }
}

/// One catalogue row with the presentation text that cannot be written inside a
/// template expression (Askama has no array-literal expressions, and the label
/// has to be built through the request's localization context anyway).
struct SettingsTaxRow {
    tax: Tax,
    /// The row's delete button label, naming the tax it would remove, so a
    /// screen reader meeting one row out of context knows which tax it acts on.
    delete_label: String,
}

/// The catalogue list the Taxes tab renders and every mutation answers with.
/// One struct, so the tab and its HTMX refresh can never disagree about what a
/// tax row looks like.
#[derive(Template)]
#[template(path = "partials/settings_taxes.html")]
struct SettingsTaxesPartial {
    rows: Vec<SettingsTaxRow>,
    localization: LocalizationContext,
}

/// The explicit confirmation step a hard delete requires.
///
/// It is a SERVER-rendered panel, not a `hx-confirm` dialog, for two reasons:
/// the operator reads which tax is about to disappear and how many references
/// still hold it, and the confirmation field it carries is what the delete
/// route actually requires — so the two-step is enforced by the server and not
/// only by the button that opens it.
#[derive(Template)]
#[template(path = "partials/settings_tax_delete_confirm.html")]
struct SettingsTaxDeleteConfirm {
    tax: Tax,
    /// The two reference counts, already formatted: they are the decision the
    /// operator is being asked to make, spelled out before they answer.
    products_summary: String,
    snapshots_summary: String,
    localization: LocalizationContext,
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
    /// `?tab=taxes` selects the Taxes tab; anything else is the business tab.
    #[serde(default)]
    tab: Option<String>,
}

async fn settings_page(
    State(state): State<AppState>,
    _: Require<SettingsManage>,
    Extension(localization): Extension<LocalizationContext>,
    principal: Extension<Principal>,
    Query(query): Query<SettingsPageQuery>,
) -> AppResult<Response> {
    let tab = SettingsTab::from_query(query.tab.as_deref());
    let (settings, locales) = state.settings_service.load().await?;
    let rows = match tab {
        SettingsTab::Taxes => tax_rows(&state, &localization).await?,
        SettingsTab::Business => Vec::new(),
    };
    let page = page_response(
        settings,
        locales,
        None,
        query.saved,
        None,
        localization,
        &principal,
        tab,
        rows,
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
        SettingsTab::Business,
        Vec::new(),
    )?;
    Ok((StatusCode::BAD_REQUEST, Html(render(page)?)).into_response())
}

/// The Taxes tab re-rendered in place, for the mutations that must answer a
/// non-HTMX submission with a real page: a redirect would drop the refusal the
/// operator has to read, and a JSON body would show them a database-shaped
/// answer instead of the sentence explaining what to do next.
async fn taxes_page_response(
    state: &AppState,
    error: Option<String>,
    localization: LocalizationContext,
    principal: &Principal,
    status: StatusCode,
) -> AppResult<Response> {
    let (settings, locales) = state.settings_service.load().await?;
    let rows = tax_rows(state, &localization).await?;
    let page = page_response(
        settings,
        locales,
        None,
        false,
        error,
        localization,
        principal,
        SettingsTab::Taxes,
        rows,
    )?;
    Ok((status, Html(render(page)?)).into_response())
}

fn page_response(
    settings: BusinessSettings,
    current_locales: Vec<BusinessLocale>,
    submitted: Option<SettingsForm>,
    saved: bool,
    error: Option<String>,
    localization: LocalizationContext,
    principal: &Principal,
    tab: SettingsTab,
    rows: Vec<SettingsTaxRow>,
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
        tab,
        rows,
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

// ---------------------------------------------------------------------------
// The Taxes tab: the canonical tax-administration surface
// ---------------------------------------------------------------------------
//
// THE URL DECISION, stated once so the next reader does not have to infer it.
//
// Tax administration has exactly ONE canonical address space, and it is this
// one: `/web/settings/taxes…`, gated by `settings.manage`, on the page at
// `/settings?tab=taxes`. Every mutation the tab issues is here, including the
// ones that do not exist anywhere else (activation and the hard delete).
//
// The pre-existing `/web/taxes`, `/web/taxes/edit` and `/web/taxes/deactivate`
// routes are NOT this surface and were deliberately left untouched. They belong
// to the Products screen's tax catalogue, they are gated by `inventory.write`,
// and the product drawer renders their fragments.
//
// WHAT IS AND IS NOT GATED BY `settings.manage`, stated exactly so the next
// reader does not over-claim:
//
//   * IRREVERSIBLE tax administration is settings-only. A hard delete — the one
//     operation that removes a definition instead of editing it — exists on no
//     other route, and `inventory.write` cannot reach it. That is what this
//     feature actually decided.
//   * The catalogue itself is NOT settings-only, and this unit did not make it
//     so. A principal holding `inventory.write` can still create, rename,
//     re-rate and deactivate/activate taxes from the Products catalogue, exactly
//     as before. Whether that should narrow is a separate authorization
//     decision, not a side effect of moving the hard delete behind a gate.
//
// Merging the two surfaces would mean either handing deletion to
// `inventory.write` or breaking the product page, its catalogue and the tests
// that drive it. Two surfaces, two owners, two permissions, no shared handler.

/// The tax form the catalogue rows and the create row submit.
#[derive(Debug, Deserialize)]
pub struct SettingsTaxForm {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub rate: String,
    #[serde(default)]
    pub is_active: Option<String>,
}

/// The hard-delete form. `confirm` is REQUIRED by the route, not merely checked
/// by the template: an operator (or a stale page, or a replayed request) that
/// posts a delete without it changes nothing.
#[derive(Debug, Deserialize)]
pub struct SettingsTaxDeleteForm {
    #[serde(default)]
    pub id: Option<i64>,
    #[serde(default)]
    pub confirm: Option<String>,
}

fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .map(|value| value == "true")
        .unwrap_or(false)
}

/// A checkbox is on when its field is present; the value is checked too because
/// a hand-written or replayed form can carry anything.
fn checked(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        value == "1" || value.eq_ignore_ascii_case("on") || value.eq_ignore_ascii_case("true")
    })
}

fn tax_id(form: &SettingsTaxForm) -> AppResult<i64> {
    form.id
        .ok_or_else(|| AppError::Validation("tax id is required".into()))
}

fn parse_tax_rate(
    rate: &str,
    localization: &LocalizationContext,
) -> AppResult<rust_decimal::Decimal> {
    localization
        .parse_decimal(rate)
        .map_err(|_| AppError::Validation("invalid tax rate".into()))
}

/// The catalogue rows, each carrying the presentation text its own button
/// needs. One builder, so the full page and the HTMX list fragment can never
/// render the same tax differently.
async fn tax_rows(
    state: &AppState,
    localization: &LocalizationContext,
) -> AppResult<Vec<SettingsTaxRow>> {
    Ok(state
        .tax_service
        .list_taxes()
        .await?
        .into_iter()
        .map(|tax| SettingsTaxRow {
            delete_label: localization.tr_with(
                MessageKey::TaxDeleteRowAction,
                &[("code", tax.code.as_str())],
            ),
            tax,
        })
        .collect())
}

async fn taxes_html(state: &AppState, localization: &LocalizationContext) -> AppResult<String> {
    SettingsTaxesPartial {
        rows: tax_rows(state, localization).await?,
        localization: localization.clone(),
    }
    .render()
    .map_err(|error| AppError::Internal(error.to_string()))
}

/// The answer to every successful catalogue mutation: the refreshed list for an
/// HTMX swap, the Taxes tab itself for a plain form post.
async fn taxes_mutation_response(
    state: &AppState,
    headers: &HeaderMap,
    localization: &LocalizationContext,
) -> AppResult<Response> {
    if is_htmx(headers) {
        let mut response = Html(taxes_html(state, localization).await?).into_response();
        response
            .headers_mut()
            .insert("HX-Trigger", "settings-taxes-changed".parse().unwrap());
        return Ok(response);
    }
    Ok(Redirect::to("/settings?tab=taxes").into_response())
}

/// The answer to a REFUSED catalogue mutation.
///
/// Only a DECISION is answered here. `refusal_status` decides which refusals
/// those are, and a fault falls straight through to the app's own
/// `AppError: IntoResponse` — see [`refusal_status`].
async fn taxes_refusal_response(
    state: &AppState,
    headers: &HeaderMap,
    error: AppError,
    localization: &LocalizationContext,
    principal: &Principal,
) -> AppResult<Response> {
    let Some(status) = refusal_status(&error) else {
        return Err(error);
    };
    let message = tax_error_message(&error, localization);
    if is_htmx(headers) {
        // The app's own JSON error shape, which `base.html`'s
        // `htmx:responseError` handler already turns into a notice — with the
        // status the refusal really has, not a flattened one.
        return Ok((status, Json(json!({ "error": message }))).into_response());
    }
    taxes_page_response(
        state,
        Some(message),
        localization.clone(),
        principal,
        status,
    )
    .await
}

/// The status a REFUSED mutation answers with, or `None` for anything this
/// function must not restyle.
///
/// The three decision variants keep the statuses the rest of the application
/// already gives them, so a caller cannot tell a tax route from any other:
///
/// * `Conflict` → 409 — the catalogue said no (a duplicate code, a tax that is
///   still referenced). The sentence says what to do about it.
/// * `NotFound` → 404 — the tax named by the request is not there.
/// * `Validation` → 400 — the submission is malformed. The sentence names the
///   field.
///
/// Everything else is `None`, and that is the point. `Database` and `Internal`
/// are FAULTS, not refusals: they are handed back untouched so the app's
/// single `AppError: IntoResponse` answers them the way every other route
/// does — `tracing::error!` for a database error, 500 for both, and a generic
/// body that cannot carry SQLite or trigger text. Mapping them to 400 here
/// would report a broken request as a decision the operator caused and could
/// act on, and would bury a database fault with no log line at all.
/// `Unauthorized`, `Forbidden` and `PayloadTooLarge` are also `None`: the gate
/// and the extractors answer those before any handler code runs, so reaching
/// this function with one would already be a defect rather than a refusal.
fn refusal_status(error: &AppError) -> Option<StatusCode> {
    match error {
        AppError::Conflict(_) => Some(StatusCode::CONFLICT),
        AppError::NotFound(_) => Some(StatusCode::NOT_FOUND),
        AppError::Validation(_) => Some(StatusCode::BAD_REQUEST),
        _ => None,
    }
}

/// Turn a domain refusal into the sentence an operator can act on.
///
/// Two rules, and the second one is the important one:
///
/// 1. The markers this application itself raises are mapped to catalog copy.
///    The two delete reasons get DIFFERENT messages on purpose: one says to
///    unlink the products, the other says the tax is kept for history and to
///    deactivate it instead — a history conflict has no remedy, and telling the
///    operator to hunt for products would be advice that cannot work.
/// 2. Anything this build does not recognize is replaced by a generic
///    localized sentence rather than forwarded. A `Validation`/`NotFound`/
///    `Conflict` payload is domain copy authored in this codebase, so it is
///    safe to show — but only when the catalogue recognizes it, because a future
///    marker would otherwise reach an operator as a bare internal string.
///    Faults never arrive here at all: [`refusal_status`] hands them back to
///    the app's own error mapping, so no SQLite or trigger text can reach a
///    caller through this function.
fn tax_error_message(error: &AppError, localization: &LocalizationContext) -> String {
    // The variant payload is the BARE message. `conflict: …`, `validation
    // error: …` and `not found: …` belong to the `Display` rendering — they are
    // thiserror's format strings, `#[error("conflict: {0}")]`, not text stored
    // in the enum — so matching the variant hands over the message alone and
    // there is no prefix to strip. (`validation_message` above does strip one,
    // because it reads `error.to_string()` rather than the enum.)
    let message = match error {
        AppError::Conflict(message)
        | AppError::Validation(message)
        | AppError::NotFound(message) => message.as_str(),
        // Unreachable through `taxes_refusal_response`, which answers faults
        // itself; kept so the function is total and can never leak a payload.
        _ => return localization.tr(MessageKey::TaxActionFailed).to_owned(),
    };
    let key = match message {
        TAX_DELETE_BLOCKED_BY_PRODUCTS => MessageKey::TaxDeleteBlockedByProducts,
        TAX_DELETE_BLOCKED_BY_HISTORY => MessageKey::TaxDeleteBlockedByHistory,
        TAX_DELETE_BLOCKED => MessageKey::TaxDeleteBlocked,
        "tax code already exists" => MessageKey::TaxCodeAlreadyExists,
        "tax code cannot be empty" | "tax code must be <= 32 chars" => {
            MessageKey::ValidationTaxCode
        }
        "tax name cannot be empty" | "tax name must be <= 128 chars" => {
            MessageKey::ValidationTaxName
        }
        "tax rate cannot be negative" => MessageKey::ValidationTaxRateNegative,
        "invalid tax rate" => MessageKey::ValidationTaxRateInvalid,
        "tax delete needs an explicit confirmation" => MessageKey::TaxDeleteConfirmationRequired,
        // A submission that never named a tax is malformed, not a decision the
        // catalogue made: the generic sentence is the honest one.
        _ => return localization.tr(MessageKey::TaxActionFailed).to_owned(),
    };
    localization.tr(key).to_owned()
}

async fn web_settings_taxes(
    State(state): State<AppState>,
    _: Require<SettingsManage>,
    Extension(localization): Extension<LocalizationContext>,
) -> AppResult<Response> {
    Ok(Html(taxes_html(&state, &localization).await?).into_response())
}

async fn web_create_tax(
    State(state): State<AppState>,
    _: Require<SettingsManage>,
    Extension(principal): Extension<Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<SettingsTaxForm>,
) -> AppResult<Response> {
    let rate = parse_tax_rate(&form.rate, &localization);
    let result = match rate {
        Ok(rate) => state
            .tax_service
            .create_tax(
                principal.user_id,
                NewTax {
                    code: form.code,
                    name: form.name,
                    rate,
                    is_active: true,
                },
            )
            .await
            .map(|_| ()),
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => taxes_mutation_response(&state, &headers, &localization).await,
        Err(error) => {
            taxes_refusal_response(&state, &headers, error, &localization, &principal).await
        }
    }
}

async fn web_edit_tax(
    State(state): State<AppState>,
    _: Require<SettingsManage>,
    Extension(principal): Extension<Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<SettingsTaxForm>,
) -> AppResult<Response> {
    let result = match (tax_id(&form), parse_tax_rate(&form.rate, &localization)) {
        (Ok(id), Ok(rate)) => state
            .tax_service
            .update_tax(
                principal.user_id,
                id,
                UpdateTax {
                    code: Some(form.code),
                    name: Some(form.name),
                    rate: Some(rate),
                    is_active: Some(checked(form.is_active.as_deref())),
                },
            )
            .await
            .map(|_| ()),
        (Err(error), _) | (_, Err(error)) => Err(error),
    };
    match result {
        Ok(()) => taxes_mutation_response(&state, &headers, &localization).await,
        Err(error) => {
            taxes_refusal_response(&state, &headers, error, &localization, &principal).await
        }
    }
}

async fn web_activate_tax(
    State(state): State<AppState>,
    _: Require<SettingsManage>,
    Extension(principal): Extension<Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<SettingsTaxForm>,
) -> AppResult<Response> {
    let result = match tax_id(&form) {
        Ok(id) => state
            .tax_service
            .activate_tax(principal.user_id, id)
            .await
            .map(|_| ()),
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => taxes_mutation_response(&state, &headers, &localization).await,
        Err(error) => {
            taxes_refusal_response(&state, &headers, error, &localization, &principal).await
        }
    }
}

async fn web_deactivate_tax(
    State(state): State<AppState>,
    _: Require<SettingsManage>,
    Extension(principal): Extension<Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<SettingsTaxForm>,
) -> AppResult<Response> {
    let result = match tax_id(&form) {
        Ok(id) => state
            .tax_service
            .deactivate_tax(principal.user_id, id)
            .await
            .map(|_| ()),
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => taxes_mutation_response(&state, &headers, &localization).await,
        Err(error) => {
            taxes_refusal_response(&state, &headers, error, &localization, &principal).await
        }
    }
}

/// The first half of the two-step delete: it changes nothing and tells the
/// operator exactly what would go, including the two reference counts that
/// decide whether the delete can happen at all.
async fn web_delete_tax_confirm(
    State(state): State<AppState>,
    _: Require<SettingsManage>,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let tax = state.tax_service.get_tax(id).await?;
    let references = state.tax_service.tax_references(id).await?;
    let count = |value: i64| value.to_string();
    let panel = SettingsTaxDeleteConfirm {
        products_summary: localization.tr_with(
            MessageKey::TaxDeleteConfirmProducts,
            &[("count", count(references.product_links).as_str())],
        ),
        snapshots_summary: localization.tr_with(
            MessageKey::TaxDeleteConfirmSnapshots,
            &[("count", count(references.document_snapshots).as_str())],
        ),
        tax,
        localization,
    }
    .render()
    .map_err(|error| AppError::Internal(error.to_string()))?;
    Ok(Html(panel).into_response())
}

/// The second half: the delete runs only when the confirmation field is present.
async fn web_delete_tax(
    State(state): State<AppState>,
    _: Require<SettingsManage>,
    Extension(principal): Extension<Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<SettingsTaxDeleteForm>,
) -> AppResult<Response> {
    let result = match (form.id, checked(form.confirm.as_deref())) {
        (Some(id), true) => state.tax_service.delete_tax(id).await,
        // No id is a malformed submission, and no confirmation is the ONE thing
        // this route refuses to guess about: neither is allowed to delete.
        (None, _) => Err(AppError::Validation("tax id is required".into())),
        (Some(_), false) => Err(AppError::Validation(
            "tax delete needs an explicit confirmation".into(),
        )),
    };
    match result {
        Ok(()) => taxes_mutation_response(&state, &headers, &localization).await,
        Err(error) => {
            taxes_refusal_response(&state, &headers, error, &localization, &principal).await
        }
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings", get(settings_page).post(settings_submit))
        .route(
            "/web/settings/taxes",
            get(web_settings_taxes).post(web_create_tax),
        )
        .route("/web/settings/taxes/edit", post(web_edit_tax))
        .route("/web/settings/taxes/activate", post(web_activate_tax))
        .route("/web/settings/taxes/deactivate", post(web_deactivate_tax))
        .route(
            "/web/settings/taxes/delete-confirm/{id}",
            get(web_delete_tax_confirm),
        )
        .route("/web/settings/taxes/delete", post(web_delete_tax))
}
