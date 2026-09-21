// Purchases web: the `/purchases` list and the `/purchases/{id}` record page,
// Askama + HTMX. Thin handlers over PurchasesService; the record body lives in
// partials/purchase_detail.html and every action posts to
// `/web/purchases/{id}/...`, so the id always comes from the URL. The old
// collection endpoints (id in the form body) stay registered for existing
// callers.
use askama::Template;
use axum::{
    extract::{Form, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{
    NewPurchase, PaymentType, PurchaseDetail, PurchaseListFilter, PurchaseRecord, PurchaseStatus,
    PurchaseSuggestions,
};
use crate::routes::AppState;
// S7 enforcement: every registered handler declares the permission its action
// needs (AC10); the collection adapters and the path handlers share ungated
// `*_impl` bodies so each registered boundary carries its own real gate. The
// mapping and its judgement calls are recorded in
// openspec/changes/2026-09-18-add-identity-module/tasks.md (S7 section).
use crate::security::authz::{
    InventoryRead, Nav, PurchasesCancel, PurchasesCreate, PurchasesRead, Require,
};

// ---------------------------------------------------------------------------
// Views + Askama templates
// ---------------------------------------------------------------------------

/// A purchase detail plus the resolved supplier name (purchases store only the id).
#[derive(Clone)]
pub struct PurchaseView {
    pub detail: PurchaseDetail,
    pub supplier_name: String,
}

#[derive(Template)]
#[template(path = "purchases.html")]
struct PurchasesTemplate {
    title: String,
    purchases: Vec<PurchaseView>,
    suggestions: PurchaseSuggestions,
    has_suggestions: bool,
    suppliers: Vec<crate::models::Supplier>,
    allow_negative: bool,
    allow_negative_stock: bool,
    today: String,
    nav_key: &'static str,
    /// Current filter values, so a bookmarkable `/purchases?supplier=…`
    /// re-renders with the same form state the server used for the list.
    filter_status: String,
    filter_supplier: String,
    filter_number: String,
    filter_from: String,
    filter_to: String,
    /// The sidebar's nav view: the entries this principal may read (S7 part 2).
    nav: Nav,
    /// Whether the acting principal may refresh the reorder suggestions
    /// (`inventory.read`, the gate the suggestions fragment and API carry).
    /// When false the page renders no Sugerido block at all — the coherent
    /// half of the old consequence where a `purchases.read`-only principal
    /// saw suggestions it could not refresh.
    show_suggestions: bool,
}

/// The `/purchases/{id}` record page. The page-header values are struct fields,
/// so the shared component and the record body read them straight from the shell.
#[derive(Template)]
#[template(path = "purchase.html")]
struct PurchasePageTemplate {
    /// Purchase number, or "Draft purchase" before confirmation.
    page_title: String,
    page_breadcrumb_label: String,
    page_breadcrumb_href: String,
    /// Empty label = no primary action (a cancelled purchase is read-only).
    page_action_href: String,
    page_action_label: String,
    record: PurchaseRecord,
    oob_picker: bool,
    method_options: Vec<crate::models::PaymentMethodWithAccount>,
    today: String,
    /// Audit display names for the record body the page includes: resolved in
    /// the wiring layer (AC20).
    created_by_name: Option<String>,
    updated_by_name: Option<String>,
    nav_key: &'static str,
    /// The sidebar's nav view: the entries this principal may read (S7 part 2).
    nav: Nav,
}

#[derive(Template)]
#[template(path = "partials/purchase_list.html")]
struct PurchaseListPartial {
    title: String,
    purchases: Vec<PurchaseView>,
}

/// The record body, shared by the page and by every action response that swaps
/// `#purchase-record`, so the action forms travel with the fragment either way.
#[derive(Template)]
#[template(path = "partials/purchase_detail.html")]
struct PurchaseDetailPartial {
    record: PurchaseRecord,
    oob_picker: bool,
    method_options: Vec<crate::models::PaymentMethodWithAccount>,
    today: String,
    /// The purchase's creator and its last editor, as display names the
    /// wiring layer resolved (never the ids).
    created_by_name: Option<String>,
    updated_by_name: Option<String>,
}

#[derive(Template)]
#[template(path = "partials/suggestion_list.html")]
struct SuggestionListPartial {
    suggestions: PurchaseSuggestions,
    has_suggestions: bool,
    today: String,
}

// ---------------------------------------------------------------------------
// Helpers (mirror sales_web)
// ---------------------------------------------------------------------------

fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .map(|v| v == "true")
        .unwrap_or(false)
}

fn parse_required_decimal(s: &str, field: &str) -> AppResult<Decimal> {
    Decimal::from_str(s.trim()).map_err(|_| AppError::Validation(format!("invalid {field}")))
}

fn parse_opt_decimal(s: &str, field: &str) -> AppResult<Option<Decimal>> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    Decimal::from_str(t)
        .map(Some)
        .map_err(|_| AppError::Validation(format!("invalid {field}")))
}

fn parse_opt_i64(s: &str, field: &str) -> AppResult<Option<i64>> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse::<i64>()
        .map(Some)
        .map_err(|_| AppError::Validation(format!("invalid {field}")))
}

fn parse_date_or_today(s: &str) -> AppResult<NaiveDate> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(chrono::Local::now().date_naive());
    }
    t.parse()
        .map_err(|_| AppError::Validation("invalid date (YYYY-MM-DD)".into()))
}

fn parse_opt_date(s: &str, field: &str) -> AppResult<Option<NaiveDate>> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse()
        .map(Some)
        .map_err(|_| AppError::Validation(format!("invalid {field} (YYYY-MM-DD)")))
}

fn clean_opt(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

async fn purchase_views(
    state: &AppState,
    filter: &PurchaseListFilter,
) -> AppResult<Vec<PurchaseView>> {
    let details = state
        .purchases_service
        .list_details_filtered(filter)
        .await?;
    let mut out = Vec::with_capacity(details.len());
    for detail in details {
        let supplier = state
            .supplier_service
            .get_supplier(detail.purchase.supplier_id)
            .await?;
        out.push(PurchaseView {
            detail,
            supplier_name: supplier.name,
        });
    }
    Ok(out)
}

fn render_list(view: Vec<PurchaseView>, title: &str) -> AppResult<Html<String>> {
    let html = PurchaseListPartial {
        title: title.to_string(),
        purchases: view,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

/// Everything the record body renders: the resolved record plus the
/// method-with-account options its action forms need. The product picker
/// searches `/web/product-search` instead of carrying the whole catalogue.
struct PurchaseRecordContext {
    record: PurchaseRecord,
    method_options: Vec<crate::models::PaymentMethodWithAccount>,
    today: String,
    /// Audit display names: the purchase's creator and its last editor (a
    /// header edit, a line change, the confirm or the cancel), resolved here
    /// in the wiring layer (AC20: the service never reads identity).
    created_by_name: Option<String>,
    updated_by_name: Option<String>,
}

async fn record_context(state: &AppState, purchase_id: i64) -> AppResult<PurchaseRecordContext> {
    let record = state.purchases_service.get_record(purchase_id).await?;
    let method_options = state.payment_method_service.methods_with_accounts().await?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut actor_ids = vec![record.purchase.created_by];
    actor_ids.extend(record.purchase.updated_by);
    let names = crate::routes::audit_actor_names(&state.pool, &actor_ids).await?;
    let name_for = |id: i64| names.get(&id).cloned();
    let created_by_name = name_for(record.purchase.created_by);
    let updated_by_name = record.purchase.updated_by.and_then(name_for);
    Ok(PurchaseRecordContext {
        record,
        method_options,
        today,
        created_by_name,
        updated_by_name,
    })
}

fn render_record(context: PurchaseRecordContext, oob_picker: bool) -> AppResult<Html<String>> {
    let html = PurchaseDetailPartial {
        record: context.record,
        oob_picker,
        method_options: context.method_options,
        today: context.today,
        created_by_name: context.created_by_name,
        updated_by_name: context.updated_by_name,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

/// Record-body response that keeps the cross-region `purchase-changed` refresh
/// event, so the subscribed list region updates after an action.
async fn changed(state: &AppState, purchase_id: i64) -> AppResult<Response> {
    changed_with_picker(state, purchase_id, false).await
}

/// Line-add response: the same body plus the out-of-band picker, empty and
/// focused, so the scanner can feed the next line without a click.
async fn changed_with_picker(
    state: &AppState,
    purchase_id: i64,
    oob_picker: bool,
) -> AppResult<Response> {
    let html = render_record(record_context(state, purchase_id).await?, oob_picker)?.0;
    let mut resp = Html(html).into_response();
    resp.headers_mut()
        .insert("HX-Trigger", "purchase-changed".parse().unwrap());
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Page + fragments
// ---------------------------------------------------------------------------

/// The purchases page is a single `purchases.read` gate. The supplier roster
/// (`suppliers.read` data) stays server-rendered for the create dialog — the
/// recorded deliberate consequence: a purchases-only principal sees the
/// roster it needs to record a purchase, and the supplier screens themselves
/// refuse it. The reorder suggestions are the coherent half: the block now
/// renders only when the principal holds `inventory.read`, the same gate the
/// suggestions fragment and API carry, so a purchases-only principal sees no
/// Sugerido block it could not refresh (S7 part 2 closed the consequence the
/// part 1 review recorded).
async fn purchases_page(
    State(state): State<AppState>,
    _: Require<PurchasesRead>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Query(query): Query<PurchaseListQuery>,
) -> Result<Html<String>, AppError> {
    let purchases = purchase_views(&state, &query.to_filter()).await?;
    // The suggestion block renders only when the principal may refresh it:
    // the fragment (`/web/purchases/suggestions`) and the API twin are gated
    // `inventory.read` because the suggestion is stock-derived data, so the
    // server-rendered block obeys the same gate. A purchases-only principal
    // sees the purchases list without the Sugerido section, never a block
    // that answers 403 on refresh.
    let show_suggestions = principal.has_permission::<InventoryRead>();
    let (suggestions, has_suggestions) = if show_suggestions {
        let suggestions = state.purchases_service.suggestions().await?;
        let has = !suggestions.suggestions.is_empty() || !suggestions.without_supplier.is_empty();
        (suggestions, has)
    } else {
        (PurchaseSuggestions::default(), false)
    };
    let suppliers = state.supplier_service.list_suppliers().await?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let tmpl = PurchasesTemplate {
        title: "All purchases".to_string(),
        purchases,
        suggestions,
        has_suggestions,
        suppliers,
        allow_negative: state.allow_negative,
        allow_negative_stock: state.allow_negative_stock,
        today,
        nav_key: "purchases",
        filter_status: query.status.trim().to_string(),
        filter_supplier: query.supplier.trim().to_string(),
        filter_number: query.number.trim().to_string(),
        filter_from: query.from.trim().to_string(),
        filter_to: query.to.trim().to_string(),
        nav: Nav::for_principal(&principal),
        show_suggestions,
    };
    Ok(Html(
        tmpl.render().map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

/// Query parameters for the purchases list filter, the same shape as
/// [`SaleListQuery`](crate::routes::sales_web::SaleListQuery). The document number
/// matches partially, because a user remembers a fragment of it.
#[derive(Debug, Deserialize, Default)]
pub struct PurchaseListQuery {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub supplier: String,
    #[serde(default)]
    pub number: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
}

impl PurchaseListQuery {
    fn to_filter(&self) -> PurchaseListFilter {
        PurchaseListFilter {
            status: parse_optional_status(&self.status),
            supplier: clean_filter_text(&self.supplier),
            supplier_ids: None,
            number: clean_filter_text(&self.number),
            from: parse_optional_date_filter(&self.from),
            to: parse_optional_date_filter(&self.to),
        }
    }
}

fn clean_filter_text(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn parse_optional_status(raw: &str) -> Option<PurchaseStatus> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse().ok()
}

fn parse_optional_date_filter(raw: &str) -> Option<NaiveDate> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse().ok()
}

/// `/purchases/{id}`: a real page inside the shell. The label is the purchase
/// number or its draft state, and the single header action slot mirrors the
/// status. Deliberate single-gate consequence (same contract as S6's customer
/// statement): the record renders only THAT purchase's own data, so the gate
/// is `purchases.read` alone — a purchases-only principal never reaches the
/// purchases list or another supplier's documents.
async fn purchase_record_page(
    State(state): State<AppState>,
    _: Require<PurchasesRead>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
) -> Result<Html<String>, AppError> {
    let context = record_context(&state, id).await?;
    let label = match &context.record.purchase.purchase_number {
        Some(number) => number.clone(),
        None => "Draft purchase".to_string(),
    };
    let (action_href, action_label) = if context.record.purchase.status == PurchaseStatus::Draft {
        ("#add-line".to_string(), "Add line".to_string())
    } else if context.record.purchase.status == PurchaseStatus::Confirmed
        && context.record.purchase.payment_type == PaymentType::Credit
    {
        ("#record-payment".to_string(), "Record payment".to_string())
    } else {
        (String::new(), String::new())
    };
    let tmpl = PurchasePageTemplate {
        page_title: label,
        page_breadcrumb_label: "Purchases".to_string(),
        page_breadcrumb_href: "/purchases".to_string(),
        page_action_href: action_href,
        page_action_label: action_label,
        record: context.record,
        oob_picker: false,
        method_options: context.method_options,
        today: context.today,
        created_by_name: context.created_by_name,
        updated_by_name: context.updated_by_name,
        nav_key: "purchases",
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(
        tmpl.render().map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

async fn web_purchase_list(
    State(state): State<AppState>,
    _: Require<PurchasesRead>,
    Query(query): Query<PurchaseListQuery>,
) -> AppResult<Response> {
    let view = purchase_views(&state, &query.to_filter()).await?;
    Ok(render_list(view, "All purchases")?.into_response())
}

async fn web_purchase_detail(
    State(state): State<AppState>,
    _: Require<PurchasesRead>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let html = render_record(record_context(&state, id).await?, false)?.0;
    Ok(Html(html).into_response())
}

/// `DELETE /web/purchases/{id}`: the documents drawer's draft delete — the
/// mirror of the sale flow. The same house shape as the other HTMX writes:
/// an empty 200 whose `HX-Trigger` tells the listening pages to re-read the
/// feed; the business outcome lives in the service, the route only answers.
async fn web_delete_draft(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    state.purchases_service.delete_draft(id).await?;
    let mut resp = Html("".to_string()).into_response();
    resp.headers_mut()
        .insert("HX-Trigger", "purchase-changed".parse().unwrap());
    Ok(resp)
}

/// The suggestions fragment is stock-derived data (the list the reorder
/// panel renders): `inventory.read`, the same gate as its JSON twin. The
/// judgement call and its cost are recorded in the S7 mapping.
async fn web_purchase_suggestions(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
) -> AppResult<Html<String>> {
    let suggestions = state.purchases_service.suggestions().await?;
    let has_suggestions =
        !suggestions.suggestions.is_empty() || !suggestions.without_supplier.is_empty();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let html = SuggestionListPartial {
        suggestions,
        has_suggestions,
        today,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

// ---------------------------------------------------------------------------
// Forms (HTMX, mirror sales_web patterns)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreatePurchaseForm {
    pub supplier_id: i64,
    #[serde(default)]
    pub payment_type: String,
    #[serde(default)]
    pub purchase_date: String,
    #[serde(default)]
    pub due_date: String,
    #[serde(default)]
    pub supplier_invoice_no: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize)]
pub struct AddLineForm {
    #[serde(default)]
    pub purchase_id: i64,
    /// An explicit id arrives from a clicked result; a scan arrives as `product`.
    #[serde(default)]
    pub product_id: Option<i64>,
    /// The typed or scanned value. The inventory service resolves it: exact
    /// barcode, then exact SKU (case-insensitive), then a numeric id.
    #[serde(default)]
    pub product: String,
    #[serde(default)]
    pub qty: String,
    #[serde(default)]
    pub unit_cost: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateLineForm {
    #[serde(default)]
    pub qty: String,
    #[serde(default)]
    pub unit_cost: String,
}

#[derive(Debug, Deserialize)]
pub struct ConfirmPurchaseForm {
    #[serde(default)]
    pub purchase_id: i64,
    #[serde(default)]
    pub method_id: String,
}

#[derive(Debug, Deserialize)]
pub struct RecordPaymentForm {
    #[serde(default)]
    pub purchase_id: i64,
    pub method_id: i64,
    #[serde(default)]
    pub amount: String,
    #[serde(default)]
    pub date: String,
}

#[derive(Debug, Deserialize)]
pub struct CancelPurchaseForm {
    #[serde(default)]
    pub purchase_id: i64,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePurchaseHeaderForm {
    #[serde(default)]
    pub purchase_date: String,
    #[serde(default)]
    pub due_date: String,
    #[serde(default)]
    pub supplier_invoice_no: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize)]
pub struct SeedSuggestionForm {
    pub product_id: i64,
    #[serde(default)]
    pub payment_type: String,
    #[serde(default)]
    pub purchase_date: String,
    #[serde(default)]
    pub due_date: String,
}

fn parse_payment_type(raw: &str) -> AppResult<PaymentType> {
    if raw.trim().is_empty() {
        Ok(PaymentType::Cash)
    } else {
        raw.parse().map_err(AppError::Validation)
    }
}

async fn web_create_purchase(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Form(form): Form<CreatePurchaseForm>,
) -> AppResult<Response> {
    let purchase = state
        .purchases_service
        .create_draft(principal.user_id, NewPurchase {
            supplier_id: form.supplier_id,
            payment_type: parse_payment_type(&form.payment_type)?,
            purchase_date: parse_date_or_today(&form.purchase_date)?,
            due_date: parse_opt_date(&form.due_date, "due_date")?,
            supplier_invoice_no: clean_opt(&form.supplier_invoice_no),
            notes: clean_opt(&form.notes),
        })
        .await?;
    let location = format!("/purchases/{}", purchase.id);
    if is_htmx(&headers) {
        // AC4: htmx performs a real navigation to the new record, so an id is
        // never typed and the back button keeps working.
        return Response::builder()
            .status(StatusCode::OK)
            .header("HX-Redirect", location)
            .body(axum::body::Body::empty())
            .map_err(|e| AppError::Internal(e.to_string()));
    }
    Ok(Redirect::to(&location).into_response())
}

async fn web_add_line(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<AddLineForm>,
) -> AppResult<Response> {
    web_add_line_impl(state, principal.user_id, headers, id, form).await
}

async fn web_add_line_impl(
    state: AppState,
    actor: i64,
    headers: HeaderMap,
    id: i64,
    form: AddLineForm,
) -> AppResult<Response> {
    let qty = parse_required_decimal(&form.qty, "qty")?;
    let unit_cost = parse_opt_decimal(&form.unit_cost, "unit_cost")?;
    // An explicit product id (a clicked result) wins over the typed text; a scan
    // or an Enter carries only the value and resolves through inventory.
    let product_id = match form.product_id.filter(|id| *id > 0) {
        Some(id) => id,
        None => state
            .inventory_service
            .resolve_product_ref(&form.product)
            .await?
            .id,
    };
    state
        .purchases_service
        .add_line(actor, id, product_id, qty, unit_cost)
        .await?;
    if is_htmx(&headers) {
        return changed_with_picker(&state, id, true).await;
    }
    Ok(Redirect::to(&format!("/purchases/{id}")).into_response())
}

/// Collection adapter: the typed-id form posts the purchase id in the body and
/// delegates to the path-based handler, so both URL shapes keep working.
async fn web_add_line_collection(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Form(form): Form<AddLineForm>,
) -> AppResult<Response> {
    web_add_line_impl(state, principal.user_id, headers, form.purchase_id, form).await
}

async fn web_update_line(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Path((purchase_id, line_id)): Path<(i64, i64)>,
    Form(form): Form<UpdateLineForm>,
) -> AppResult<Response> {
    let qty = parse_required_decimal(&form.qty, "qty")?;
    let unit_cost = parse_required_decimal(&form.unit_cost, "unit_cost")?;
    state
        .purchases_service
        .update_line(principal.user_id, line_id, qty, unit_cost)
        .await?;
    if is_htmx(&headers) {
        return changed(&state, purchase_id).await;
    }
    Ok(Redirect::to(&format!("/purchases/{purchase_id}")).into_response())
}

async fn web_remove_line(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path((purchase_id, line_id)): Path<(i64, i64)>,
) -> AppResult<Response> {
    state.purchases_service.remove_line(principal.user_id, line_id).await?;
    changed(&state, purchase_id).await
}

async fn web_confirm_purchase(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<ConfirmPurchaseForm>,
) -> AppResult<Response> {
    web_confirm_purchase_impl(state, principal.user_id, headers, id, form).await
}

async fn web_confirm_purchase_impl(
    state: AppState,
    actor: i64,
    headers: HeaderMap,
    id: i64,
    form: ConfirmPurchaseForm,
) -> AppResult<Response> {
    let method_id = parse_opt_i64(&form.method_id, "method_id")?;
    state.purchases_service.confirm(actor, id, method_id).await?;
    if is_htmx(&headers) {
        return changed(&state, id).await;
    }
    Ok(Redirect::to(&format!("/purchases/{id}")).into_response())
}

async fn web_confirm_purchase_collection(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Form(form): Form<ConfirmPurchaseForm>,
) -> AppResult<Response> {
    web_confirm_purchase_impl(state, principal.user_id, headers, form.purchase_id, form).await
}

async fn web_record_payment(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<RecordPaymentForm>,
) -> AppResult<Response> {
    web_record_payment_impl(state, principal.user_id, headers, id, form).await
}

async fn web_record_payment_impl(
    state: AppState,
    actor: i64,
    headers: HeaderMap,
    id: i64,
    form: RecordPaymentForm,
) -> AppResult<Response> {
    let amount = parse_required_decimal(&form.amount, "amount")?;
    let date = parse_date_or_today(&form.date)?;
    state
        .purchases_service
        .record_payment(actor, id, form.method_id, amount, date)
        .await?;
    if is_htmx(&headers) {
        return changed(&state, id).await;
    }
    Ok(Redirect::to(&format!("/purchases/{id}")).into_response())
}

async fn web_record_payment_collection(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Form(form): Form<RecordPaymentForm>,
) -> AppResult<Response> {
    web_record_payment_impl(state, principal.user_id, headers, form.purchase_id, form).await
}

async fn web_cancel_purchase(
    State(state): State<AppState>,
    _: Require<PurchasesCancel>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CancelPurchaseForm>,
) -> AppResult<Response> {
    web_cancel_purchase_impl(state, principal.user_id, headers, id, form).await
}

async fn web_cancel_purchase_impl(
    state: AppState,
    actor: i64,
    headers: HeaderMap,
    id: i64,
    form: CancelPurchaseForm,
) -> AppResult<Response> {
    state
        .purchases_service
        .cancel(actor, id, clean_opt(&form.reason))
        .await?;
    if is_htmx(&headers) {
        return changed(&state, id).await;
    }
    Ok(Redirect::to(&format!("/purchases/{id}")).into_response())
}

async fn web_cancel_purchase_collection(
    State(state): State<AppState>,
    _: Require<PurchasesCancel>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Form(form): Form<CancelPurchaseForm>,
) -> AppResult<Response> {
    web_cancel_purchase_impl(state, principal.user_id, headers, form.purchase_id, form).await
}

/// Edit the draft header in place (dates, invoice, notes); the supplier and the
/// payment type stay fixed at creation, as the service enforces.
async fn web_update_purchase_header(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<UpdatePurchaseHeaderForm>,
) -> AppResult<Response> {
    let purchase_date = parse_opt_date(&form.purchase_date, "purchase_date")?;
    let due_date = parse_opt_date(&form.due_date, "due_date")?;
    state
        .purchases_service
        .update_draft(
            principal.user_id,
            id,
            crate::models::UpdatePurchaseDraft {
                purchase_date,
                due_date: Some(due_date),
                supplier_invoice_no: Some(clean_opt(&form.supplier_invoice_no)),
                notes: Some(form.notes),
                ..Default::default()
            },
        )
        .await?;
    if is_htmx(&headers) {
        return changed(&state, id).await;
    }
    Ok(Redirect::to(&format!("/purchases/{id}")).into_response())
}

/// Seed a Draft pedido from one suggested low-stock product: the service
/// re-derives the suggestion (chosen supplier, qty, satellite cost) so the form
/// never decides business values.
/// One draft pedido seeded from one suggested product: the suggestion is
/// re-derived by the service (a service composition, never a permission
/// grant), and the document it creates is a purchase — `purchases.create`.
async fn web_seed_from_suggestion(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Form(form): Form<SeedSuggestionForm>,
) -> AppResult<Response> {
    let suggestions = state.purchases_service.suggestions().await?;
    let item = suggestions
        .suggestions
        .into_iter()
        .find(|s| s.product.id == form.product_id)
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "suggestion for product {} not found",
                form.product_id
            ))
        })?;
    if item.suggested_qty <= Decimal::ZERO {
        return Err(AppError::Validation(
            "suggested qty must be > 0 to seed a draft".into(),
        ));
    }
    let purchase = state
        .purchases_service
        .create_draft(principal.user_id, NewPurchase {
            supplier_id: item.supplier_id,
            payment_type: parse_payment_type(&form.payment_type)?,
            purchase_date: parse_date_or_today(&form.purchase_date)?,
            due_date: parse_opt_date(&form.due_date, "due_date")?,
            supplier_invoice_no: None,
            notes: None,
        })
        .await?;
    state
        .purchases_service
        .add_line(
            principal.user_id,
            purchase.id,
            item.product.id,
            item.suggested_qty,
            Some(item.unit_cost),
        )
        .await?;
    if is_htmx(&headers) {
        // The seeded draft opens its record, so the suggestion ends on the page
        // where its line can be reviewed and confirmed.
        return Response::builder()
            .status(StatusCode::OK)
            .header("HX-Redirect", format!("/purchases/{}", purchase.id))
            .body(axum::body::Body::empty())
            .map_err(|e| AppError::Internal(e.to_string()));
    }
    Ok(Redirect::to(&format!("/purchases/{}", purchase.id)).into_response())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/purchases", get(purchases_page))
        .route("/purchases/{id}", get(purchase_record_page))
        .route(
            "/web/purchases",
            get(web_purchase_list).post(web_create_purchase),
        )
        .route("/web/purchases/suggestions", get(web_purchase_suggestions))
        .route(
            "/web/purchases/from-suggestion",
            post(web_seed_from_suggestion),
        )
        .route("/web/purchases/lines", post(web_add_line_collection))
        .route(
            "/web/purchases/confirm",
            post(web_confirm_purchase_collection),
        )
        .route(
            "/web/purchases/payments",
            post(web_record_payment_collection),
        )
        .route(
            "/web/purchases/cancel",
            post(web_cancel_purchase_collection),
        )
        .route("/web/purchases/{id}", get(web_purchase_detail).delete(web_delete_draft))
        .route("/web/purchases/{id}/lines", post(web_add_line))
        .route(
            "/web/purchases/{purchase_id}/lines/{line_id}",
            post(web_update_line).delete(web_remove_line),
        )
        .route(
            "/web/purchases/{id}/header",
            post(web_update_purchase_header),
        )
        .route("/web/purchases/{id}/confirm", post(web_confirm_purchase))
        .route("/web/purchases/{id}/payments", post(web_record_payment))
        .route("/web/purchases/{id}/cancel", post(web_cancel_purchase))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    use tower::ServiceExt;

    use crate::models::PaymentType;
    use crate::routes::AppState;
    use crate::security::test_support;

    /// A valid acting user for the mechanical call sites: the migration's
    /// sentinel account (the system actor pre-existing rows are attributed to).
    /// The audit-attribution tests seed their own users instead, because there
    /// the point is telling two actors apart.
    async fn audit_actor(state: &AppState) -> i64 {
        test_support::audit_actor_id(&state.pool).await.unwrap()
    }

    async fn test_state() -> AppState {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        // S1b part 1: seed the fixed test session every request will authenticate with.
        test_support::seed_session(&pool).await.unwrap();
        AppState::new(pool, false, true)
    }

    async fn get_html(app: axum::Router, uri: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn post_json(
        app: axum::Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    async fn post_form(app: axum::Router, uri: &str, body: &str) -> StatusCode {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body.to_string()))
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    async fn delete_html(app: axum::Router, uri: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .method("DELETE")
            .uri(uri)
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn get_json(app: axum::Router, uri: &str) -> serde_json::Value {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    /// POST like the record page does, keeping the response status, the
    /// `HX-Redirect` header and the body for assertions.
    async fn post_form_response(
        app: axum::Router,
        uri: &str,
        body: &str,
    ) -> (StatusCode, Option<String>, String) {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let redirect = resp
            .headers()
            .get("HX-Redirect")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, redirect, String::from_utf8_lossy(&bytes).to_string())
    }

    /// The opening tag that carries `needle`, for attribute assertions such as
    /// `hx-confirm` on the cancel control.
    fn element_tag_containing<'a>(html: &'a str, needle: &str) -> &'a str {
        let pos = html
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} not rendered: {html:.600}"));
        let start = html[..pos]
            .rfind('<')
            .expect("the attribute must sit inside a tag");
        let end = pos + html[pos..].find('>').expect("unterminated tag");
        &html[start..=end]
    }

    /// A row's rendered HTML, sliced from its `id` to the closing `</tr>`.
    fn row_with_id<'a>(html: &'a str, id: &str) -> &'a str {
        let start = html
            .find(&format!("id=\"{id}\""))
            .unwrap_or_else(|| panic!("row {id} not rendered: {html:.600}"));
        let after = &html[start..];
        let end = after
            .find("</tr>")
            .unwrap_or_else(|| panic!("row {id} has no closing tag"));
        &after[..end]
    }

    /// Cuts the `<form>...</form>` region that contains `needle`, for structural
    /// assertions such as "the results container is not inside the picker form".
    fn enclosing_form<'a>(html: &'a str, needle: &str) -> &'a str {
        let pos = html
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} not rendered: {html:.600}"));
        let start = html[..pos].rfind("<form").expect("needle must sit in a form");
        let end = html[pos..].find("</form>").expect("form must close");
        &html[start..pos + end + "</form>".len()]
    }

    /// The line response must bring the picker back out of band, empty and
    /// focused, so the next scan lands without a click.
    fn assert_oob_picker_is_empty_and_focused(html: &str) {
        let oob_pos = html
            .find("hx-swap-oob=\"true\"")
            .unwrap_or_else(|| panic!("the picker must come back out of band: {html:.800}"));
        let tag_start = html[..oob_pos].rfind('<').unwrap();
        let tag_end = oob_pos + html[oob_pos..].find('>').unwrap();
        let oob_tag = &html[tag_start..=tag_end];
        assert!(oob_tag.contains("id=\"line-picker\""), "{oob_tag}");
        let oob = &html[tag_start..];
        assert!(
            oob.contains("autofocus"),
            "the picker must come back focused: {oob:.400}"
        );
        let input_pos = oob
            .find("id=\"product-picker\"")
            .expect("the out-of-band picker renders its field");
        let input_start = oob[..input_pos].rfind('<').unwrap();
        let input_end = input_pos + oob[input_pos..].find('>').unwrap();
        let input_tag = &oob[input_start..=input_end];
        assert!(
            !input_tag.contains("value="),
            "the picker must come back empty: {input_tag}"
        );
    }

    /// Everything a record-page test needs to address the seeded document.
    struct RecordFixture {
        purchase_id: i64,
        line_id: i64,
        product_id: i64,
        product_name: String,
        product_sku: String,
        supplier_id: i64,
        supplier_name: String,
        account_id: i64,
        method_id: i64,
        account_name: String,
        method_name: String,
    }

    /// One draft purchase with one line, plus an account configured with Cash, so a
    /// record-page test can drive draft, confirmed, paid and cancelled states.
    async fn seed_record_fixture(state: &AppState, payment_type: PaymentType) -> RecordFixture {
        use crate::models::{NewProduct, NewPurchase, NewSupplier, ProductKind};
        use chrono::NaiveDate;
        use rust_decimal::Decimal;

        let product = state
            .inventory_service
            .create_product(
                audit_actor(&state).await,
                NewProduct {
                sku: "REC-PUR".into(),
                name: "Record purchase product".into(),
                kind: ProductKind::Product,
                category_id: None,
                unit: "un".into(),
                sale_price: Decimal::from(25),
                cost_price: Decimal::from(10),
                track_stock: false,
                min_stock: None,
                max_stock: None,
                location: None,
                notes: None,
                markup_pct: None,
            })
            .await
            .unwrap();
        let supplier = state
            .supplier_service
            .create_supplier(audit_actor(&state).await, NewSupplier {
                name: "Record Supplier".into(),
                phone: None,
                notes: None,
            })
            .await
            .unwrap();
        let purchase = state
            .purchases_service
            .create_draft(audit_actor(&state).await, NewPurchase {
                supplier_id: supplier.id,
                payment_type,
                purchase_date: NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                due_date: match payment_type {
                    PaymentType::Credit => Some(NaiveDate::from_ymd_opt(2024, 6, 2).unwrap()),
                    PaymentType::Cash => None,
                },
                supplier_invoice_no: None,
                notes: None,
            })
            .await
            .unwrap();
        let line = state
            .purchases_service
            .add_line(audit_actor(&state).await, purchase.id, product.id, Decimal::from(2), None)
            .await
            .unwrap();
        let account = state.account_service.create(audit_actor(&state).await, "Caja").await.unwrap();
        // Purchase payments are Expenses; fund the account so the guard flag under
        // test is the record shape, not a zero balance.
        state
            .transaction_service

            .create(
                audit_actor(&state).await,
                account.id,
                crate::models::TransactionKind::Income,
                Decimal::from(1000),
                Some("fixture funding".into()),
                NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
            )
            .await
            .unwrap();
        state
            .payment_method_service
            .ensure_defaults_for_account(audit_actor(&state).await, account.id, "Caja")
            .await
            .unwrap();
        let method = state
            .payment_method_service
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.name == "Cash")
            .expect("Cash is seeded by migrations");

        RecordFixture {
            purchase_id: purchase.id,
            line_id: line.id,
            product_id: product.id,
            product_name: product.name,
            product_sku: product.sku,
            supplier_id: supplier.id,
            supplier_name: supplier.name,
            account_id: account.id,
            method_id: method.id,
            account_name: account.name,
            method_name: method.name,
        }
    }

    /// A second product for scan and click flows, so the fixture's own line is
    /// never repeated by accident.
    async fn seed_extra_product(
        state: &AppState,
        sku: &str,
        barcode: Option<&str>,
    ) -> crate::models::Product {
        use crate::models::{NewProduct, ProductKind};
        use rust_decimal::Decimal;

        let product = state
            .inventory_service
            .create_product(
                audit_actor(&state).await,
                NewProduct {
                sku: sku.into(),
                name: format!("prod {sku}"),
                kind: ProductKind::Product,
                category_id: None,
                unit: "un".into(),
                sale_price: Decimal::from(25),
                cost_price: Decimal::from(10),
                track_stock: false,
                min_stock: None,
                max_stock: None,
                location: None,
                notes: None,
                markup_pct: None,
            })
            .await
            .unwrap();
        if let Some(code) = barcode {
            state
                .inventory_service
                .add_barcode(product.id, code)
                .await
                .unwrap();
        }
        product
    }

    #[tokio::test]
    async fn web_draft_line_editor_add_update_remove() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let (st, v) = post_json(
            app.clone(),
            "/api/suppliers",
            serde_json::json!({ "name": "Editor Sup" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed supplier: {v}");
        let sup = v["id"].as_i64().unwrap();
        let (st, v) = post_json(
            app.clone(),
            "/api/products",
            serde_json::json!({
                "sku": "EDIT-P", "name": "prod EDIT-P", "kind": "Product",
                "unit": "un", "sale_price": "10", "cost_price": "5",
                "track_stock": true, "min_stock": "5", "max_stock": "50"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed product: {v}");
        let pid = v["id"].as_i64().unwrap();

        let body = format!("supplier_id={sup}&payment_type=Cash&purchase_date=2024-05-02");
        assert_eq!(
            post_form(app.clone(), "/web/purchases", &body).await,
            StatusCode::OK
        );
        let v = get_json(app.clone(), "/api/purchases").await;
        let purchase_id = v["purchases"][0]["purchase"]["id"].as_i64().unwrap();

        // Add a line through the Draft editor endpoint.
        let body = format!("purchase_id={purchase_id}&product_id={pid}&qty=2&unit_cost=5");
        assert_eq!(
            post_form(app.clone(), "/web/purchases/lines", &body).await,
            StatusCode::OK
        );
        let v = get_json(app.clone(), &format!("/api/purchases/{purchase_id}")).await;
        let line_id = v["lines"][0]["id"].as_i64().unwrap();
        assert_eq!(v["lines"][0]["qty"].as_str().unwrap(), "2");

        // Inline save edits qty/unit_cost in place.
        let body = "qty=3&unit_cost=6";
        assert_eq!(
            post_form(
                app.clone(),
                &format!("/web/purchases/{purchase_id}/lines/{line_id}"),
                body
            )
            .await,
            StatusCode::OK
        );
        let v = get_json(app.clone(), &format!("/api/purchases/{purchase_id}")).await;
        assert_eq!(v["lines"][0]["qty"].as_str().unwrap(), "3");
        assert_eq!(v["lines"][0]["unit_cost"].as_str().unwrap(), "6");

        // Inline remove empties the Draft again.
        let (st, html) = delete_html(
            app.clone(),
            &format!("/web/purchases/{purchase_id}/lines/{line_id}"),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            html.contains("No lines yet"),
            "detail should show the empty state: {html:.300}"
        );
        let v = get_json(app.clone(), &format!("/api/purchases/{purchase_id}")).await;
        assert!(v["lines"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn web_purchases_page_renders() {
        let app = crate::routes::router(test_state().await);
        let (status, html) = get_html(app, "/purchases").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Purchases"), "page should mention Purchases");
        assert!(html.contains("Sugerido"), "page should have the suggestion panel");
    }

    // -- N3: the purchase record page ------------------------------------------

    /// The typed-id forms are gone. The purchases list renders no `purchase_id`
    /// input, drops the side-panel detail target and links every row to its record
    /// page, so an id is never typed. (redesign-interface N3)
    #[tokio::test]
    async fn web_purchases_page_has_no_typed_id_forms_and_links_records() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state);

        let (status, html) = get_html(app, "/purchases").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !html.contains("name=\"purchase_id\""),
            "the purchases list must not ask for a typed purchase id: {html:.600}"
        );
        assert!(
            !html.contains("id=\"purchase-detail\""),
            "the side-panel detail must be gone: {html:.600}"
        );
        assert!(
            html.contains(&format!("href=\"/purchases/{}\"", fixture.purchase_id)),
            "every row must link to its record: {html:.600}"
        );
        assert!(
            html.contains("Sugerido"),
            "the suggestion panel stays on the list page: {html:.600}"
        );
    }

    /// AC4: creating a purchase answers `HX-Redirect` to its record, so htmx
    /// performs a real navigation and no id is typed.
    #[tokio::test]
    async fn web_create_purchase_redirects_to_the_record() {
        let state = test_state().await;
        let supplier = state
            .supplier_service
            .create_supplier(audit_actor(&state).await, crate::models::NewSupplier {
                name: "Redirect Sup".into(),
                phone: None,
                notes: None,
            })
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());
        let body = format!(
            "supplier_id={}&payment_type=Cash&purchase_date=2024-05-02",
            supplier.id
        );
        let (status, redirect, resp) = post_form_response(app, "/web/purchases", &body).await;
        assert_eq!(status, StatusCode::OK, "{resp}");
        let redirect = redirect.expect("AC4: the create response must carry HX-Redirect");
        assert!(redirect.starts_with("/purchases/"), "{redirect}");
        let purchase_id: i64 = redirect["/purchases/".len()..]
            .parse()
            .unwrap_or_else(|_| panic!("HX-Redirect must end in the purchase id: {redirect}"));
        let detail = state
            .purchases_service
            .get_detail(purchase_id)
            .await
            .unwrap();
        assert_eq!(detail.purchase.supplier_id, supplier.id);
    }

    /// The non-HTMX form path also lands on the record page, so a browser without
    /// htmx still never sees a list to retype an id from.
    #[tokio::test]
    async fn web_create_purchase_redirects_a_plain_form_to_the_record() {
        let state = test_state().await;
        let supplier = state
            .supplier_service
            .create_supplier(audit_actor(&state).await, crate::models::NewSupplier {
                name: "Plain Sup".into(),
                phone: None,
                notes: None,
            })
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let body = format!(
            "supplier_id={}&payment_type=Cash&purchase_date=2024-05-02",
            supplier.id
        );
        let req = Request::builder()
            .method("POST")
            .uri("/web/purchases")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
        let location = resp
            .headers()
            .get("location")
            .expect("a plain create must redirect to the record")
            .to_str()
            .unwrap()
            .to_string();
        assert!(location.starts_with("/purchases/"), "{location}");
    }

    /// AC5 + name resolution: `/purchases/{id}` is a real page inside the shell
    /// carrying the supplier name, product names and SKUs, and account and method
    /// names; an unknown id is 404 with the existing error shape.
    #[tokio::test]
    async fn web_purchase_record_page_resolves_names_and_unknown_id_is_404() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        state
            .purchases_service
            .confirm(audit_actor(&state).await, fixture.purchase_id, None)
            .await
            .unwrap();
        state
            .purchases_service

            .record_payment(audit_actor(&state).await, 
                fixture.purchase_id,
                fixture.method_id,
                Decimal::from(10),
                chrono::NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
            )
            .await
            .unwrap();
        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        let payment_id = detail.payments[0].id;
        let purchase_number = detail
            .purchase
            .purchase_number
            .clone()
            .expect("a confirmed purchase carries its number");
        let app = crate::routes::router(state);

        let (status, html) = get_html(
            app.clone(),
            &format!("/purchases/{}", fixture.purchase_id),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(html.contains("data-page-header"), "record uses the page header");
        assert!(
            html.contains("data-nav=\"purchases\"") && html.contains("aria-current=\"page\""),
            "record page keeps the purchases nav key"
        );
        assert!(html.contains(&fixture.supplier_name), "{html:.600}");
        assert!(html.contains(&purchase_number), "{html:.600}");

        let line_row = row_with_id(&html, &format!("purchase-line-{}", fixture.line_id));
        assert!(line_row.contains(&fixture.product_name), "{line_row}");
        assert!(line_row.contains(&fixture.product_sku), "{line_row}");
        assert!(!line_row.contains("product #"), "{line_row}");

        let payment_row = row_with_id(&html, &format!("purchase-payment-{payment_id}"));
        assert!(payment_row.contains(&fixture.account_name), "{payment_row}");
        assert!(payment_row.contains(&fixture.method_name), "{payment_row}");
        assert!(!payment_row.contains("account #"), "{payment_row}");
        assert!(!payment_row.contains("method #"), "{payment_row}");

        let (status, body) = get_html(app, "/purchases/999999").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("purchase 999999 not found"), "{body}");
        assert!(!body.contains("route not found"), "{body}");
    }

    /// AC6: the actions offered match the document status. A draft can add a line,
    /// edit its header, confirm and discard; a confirmed credit purchase can record
    /// payments and cancel but cannot edit lines or the header; a cancelled one is
    /// read-only and shows its reason.
    #[tokio::test]
    async fn web_purchase_record_actions_are_status_gated() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let app = crate::routes::router(state.clone());
        let base = format!("/web/purchases/{}", fixture.purchase_id);

        let (status, html) = get_html(
            app.clone(),
            &format!("/purchases/{}", fixture.purchase_id),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        for target in [
            format!("{base}/lines"),
            format!("{base}/header"),
            format!("{base}/confirm"),
            format!("{base}/cancel"),
        ] {
            assert!(
                html.contains(&target),
                "a draft must offer {target}: {html:.400}"
            );
        }
        assert!(
            !html.contains(&format!("{base}/payments")),
            "a draft must not offer payment recording"
        );

        state
            .purchases_service
            .confirm(audit_actor(&state).await, fixture.purchase_id, None)
            .await
            .unwrap();
        let (status, html) = get_html(
            app.clone(),
            &format!("/purchases/{}", fixture.purchase_id),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains(&format!("{base}/payments")),
            "a confirmed credit purchase must offer payment recording"
        );
        assert!(html.contains(&format!("{base}/cancel")));
        assert!(
            !html.contains(&format!("{base}/lines")),
            "a confirmed purchase must not edit lines"
        );
        assert!(
            !html.contains(&format!("{base}/header")),
            "a confirmed purchase must not edit its header"
        );

        state
            .purchases_service
            .cancel(audit_actor(&state).await, fixture.purchase_id, Some("wrong order".to_string()))
            .await
            .unwrap();
        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("wrong order"),
            "a cancelled purchase must show its reason: {html:.400}"
        );
        for target in [
            format!("{base}/lines"),
            format!("{base}/header"),
            format!("{base}/confirm"),
            format!("{base}/payments"),
            format!("{base}/cancel"),
        ] {
            assert!(
                !html.contains(&target),
                "a cancelled purchase must be read-only, found {target}"
            );
        }
    }

    /// Triangulation for AC6: a confirmed Cash purchase is settled at confirm, so
    /// it offers cancel but no payment form, and its lines and header stay frozen.
    #[tokio::test]
    async fn web_purchase_record_confirmed_cash_has_no_payment_form() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service

            .confirm(audit_actor(&state).await, 
                fixture.purchase_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let base = format!("/web/purchases/{}", fixture.purchase_id);

        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains(&format!("{base}/cancel")),
            "a confirmed cash purchase can still be cancelled"
        );
        assert!(
            !html.contains(&format!("{base}/payments")),
            "a confirmed cash purchase must not offer payment recording"
        );
        assert!(
            !html.contains(&format!("{base}/lines")),
            "a confirmed cash purchase must not edit lines"
        );
        assert!(
            !html.contains(&format!("{base}/header")),
            "a confirmed cash purchase must not edit its header"
        );
    }

    /// Cost-freshness T4: a draft line whose cost rose above the product's
    /// stored cost renders the stale-cost warning with BOTH numbers, as a
    /// sub-row underneath the line row (never inside it, so the line row's
    /// text order stays product · qty · cost, which the browser suite
    /// asserts as-is). The same response that adds a line swaps only
    /// `#purchase-record-money`, so the warning must ride that fragment.
    #[tokio::test]
    async fn web_purchase_record_draft_flags_a_rising_line_cost_on_the_fragment() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        // The fixture's line sits at the product's stored cost (10); push the
        // line's cost above it to reach the warning's condition.
        state
            .purchases_service
            .update_line(
                audit_actor(&state).await,
                fixture.line_id,
                Decimal::from(2),
                Decimal::from(12),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, html) = get_html(app.clone(), &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains(">stale cost<"),
            "a rising line cost must be flagged on the record page: {html:.800}"
        );
        assert!(
            html.contains("line cost $12.00"),
            "the warning must show the line's cost: {html:.800}"
        );
        assert!(
            html.contains("stored $10.00"),
            "the warning must show the stored cost it is behind: {html:.800}"
        );

        // Placement: the line row itself keeps its text order — the warning is
        // a separate sub-row after it, still inside the lines table.
        let line_row = row_with_id(&html, &format!("purchase-line-{}", fixture.line_id));
        assert!(
            !line_row.contains("stale cost"),
            "the warning must not sit inside the line row: {line_row}"
        );
        let after_line_row = &html[html.find(line_row).unwrap() + line_row.len()..];
        let table_end = after_line_row
            .find("</tbody>")
            .expect("the lines table must close");
        assert!(
            after_line_row[..table_end].contains(">stale cost<"),
            "the warning must render as a sub-row right below its line: {}",
            &after_line_row[..table_end]
        );

        // Adding a line swaps only `#purchase-record-money`: the warning must
        // travel inside that fragment, so a newly added stale line is flagged
        // by the very response that adds it. The second product keeps the
        // first line's warning on screen (one product, one line per purchase).
        let second = seed_extra_product(&state, "COST-RISE", None).await;
        let (status, _, added) = post_form_response(
            app,
            &format!("/web/purchases/{}/lines", fixture.purchase_id),
            &format!(
                "product=record&qty=1&unit_cost=15&product_id={}",
                second.id
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{added}");
        assert!(
            added.contains(">stale cost<"),
            "the add-line response must carry the warning: {added:.800}"
        );
        assert!(
            added.contains("line cost $15.00") && added.contains("stored $10.00"),
            "the fragment must show both numbers: {added:.800}"
        );
    }

    /// Cost-freshness T4 triangulation: an equal cost is not stale, so the
    /// draft renders no warning at all.
    #[tokio::test]
    async fn web_purchase_record_draft_hides_the_cost_warning_when_costs_are_equal() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state);

        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        assert!(
            !html.contains("stale cost"),
            "an equal cost is not stale: {html:.800}"
        );
    }

    /// Cost-freshness T4 triangulation: the warning is draft-only. The action
    /// that follows it only exists while the purchase is editable, and the
    /// product drawer already carries the permanent badge for a confirmed
    /// purchase, so a confirmed document renders no warning even with a
    /// genuinely rising line cost.
    #[tokio::test]
    async fn web_purchase_record_confirmed_purchase_hides_the_rising_cost_warning() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .update_line(
                audit_actor(&state).await,
                fixture.line_id,
                Decimal::from(2),
                Decimal::from(12),
            )
            .await
            .unwrap();
        state
            .purchases_service
            .confirm(
                audit_actor(&state).await,
                fixture.purchase_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state);

        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        assert!(
            !html.contains("stale cost"),
            "a confirmed purchase must not carry the draft-only warning: {html:.800}"
        );
    }

    /// Triangulation for the status gate: the template is presentation only.
    /// Posting the hidden draft actions directly at a confirmed purchase still
    /// reaches the service, which refuses them (400) and leaves the document
    /// untouched.
    #[tokio::test]
    async fn web_purchase_record_confirmed_refuses_draft_actions_at_the_service() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service

            .confirm(audit_actor(&state).await, 
                fixture.purchase_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());
        let base = format!("/web/purchases/{}", fixture.purchase_id);

        // Header edit: refused, and the header keeps its values.
        let (status, _, body) = post_form_response(
            app.clone(),
            &format!("{base}/header"),
            "purchase_date=2024-05-03&due_date=&supplier_invoice_no=X&notes=nope",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(detail.purchase.purchase_date.to_string(), "2024-05-02");
        assert!(detail.purchase.notes.is_empty());

        // Line add: the same route that works on a draft is refused by the service.
        let (status, _, body) = post_form_response(
            app,
            &format!("{base}/lines"),
            &format!("product={}&qty=1&unit_cost=", fixture.product_sku),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let after = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(after.lines.len(), detail.lines.len());
    }

    /// AC7: cancelling asks for confirmation before the request is sent; the
    /// confirm control carries `hx-confirm`. The same holds for discarding a draft.
    #[tokio::test]
    async fn web_purchase_record_cancel_asks_for_confirmation() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let app = crate::routes::router(state.clone());

        let cancel_needle = format!("hx-post=\"/web/purchases/{}/cancel\"", fixture.purchase_id);
        let (status, html) = get_html(
            app.clone(),
            &format!("/purchases/{}", fixture.purchase_id),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let discard = element_tag_containing(&html, &cancel_needle);
        assert!(
            discard.contains("hx-confirm"),
            "discarding a draft must ask first: {discard}"
        );

        state
            .purchases_service
            .confirm(audit_actor(&state).await, fixture.purchase_id, None)
            .await
            .unwrap();
        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK);
        let cancel = element_tag_containing(&html, &cancel_needle);
        assert!(
            cancel.contains("hx-confirm"),
            "cancelling a confirmed purchase must ask first: {cancel}"
        );
    }

    /// The record-page actions swap the record body and keep the
    /// `purchase-changed` refresh event, so the URL stays stable and subscribed
    /// regions update.
    #[tokio::test]
    async fn web_purchase_record_header_edit_swaps_the_body_and_triggers_refresh() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());
        let body = "purchase_date=2024-05-03&due_date=&supplier_invoice_no=A-9&notes=edited+note";
        let req = Request::builder()
            .method("POST")
            .uri(format!("/web/purchases/{}/header", fixture.purchase_id))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("HX-Trigger").map(|v| v.to_str().unwrap()),
            Some("purchase-changed")
        );
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&bytes).to_string();
        assert!(html.contains("purchase-record-inner"), "{html:.400}");
        assert!(html.contains("edited note"), "{html:.400}");
        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(detail.purchase.notes, "edited note");
        assert_eq!(detail.purchase.purchase_date.to_string(), "2024-05-03");
        assert_eq!(detail.purchase.supplier_invoice_no.as_deref(), Some("A-9"));
    }

    // -- N4: the product picker on the purchase record page --------------------

    /// The catalogue `<select>` is replaced by one field that searches with a
    /// debounce, submits on Enter and clears on Escape; the results container is a
    /// sibling of the form, and every result is its own add action against the
    /// purchase line endpoint.
    #[tokio::test]
    async fn n4_purchase_record_offers_the_picker_instead_of_the_catalogue_select() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state);

        let (status, html) = get_html(app, &format!("/purchases/{}", fixture.purchase_id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            !html.contains("<select name=\"product_id\""),
            "the whole-catalogue select must be gone: {html:.800}"
        );

        let picker = enclosing_form(&html, "id=\"product-picker\"");
        assert!(
            picker.contains(&format!("hx-post=\"/web/purchases/{}/lines\"", fixture.purchase_id)),
            "{picker}"
        );
        assert!(
            picker.contains("data-action=\"Add line\""),
            "the notice must name the failed line action: {picker}"
        );
        assert!(picker.contains("hx-get=\"/web/product-search\""), "{picker}");
        assert!(
            picker.contains("delay:"),
            "the search must be debounced: {picker}"
        );
        assert!(
            picker.contains("hx-target=\"#product-search-results\""),
            "{picker}"
        );
        assert!(
            picker.contains("hx-on:keyup")
                && picker.contains("Escape")
                && picker.contains("this.value"),
            "Escape must clear the field declaratively: {picker}"
        );
        assert!(
            picker.contains("name=\"qty\"") && picker.contains("value=\"1\""),
            "a scan and a click must both carry the default quantity: {picker}"
        );
        assert!(
            !picker.contains("id=\"product-search-results\""),
            "the results container must be a sibling of the picker form, never inside it: {picker}"
        );
        assert!(
            html.contains("id=\"product-search-results\""),
            "the page must render the sibling results container: {html:.600}"
        );
        assert!(
            html.contains("id=\"purchase-record-money\""),
            "adding a line swaps the money region, which carries the total and the lines"
        );
    }

    /// AC9 + AC10: an exact barcode submits the line in one step, the same response
    /// carries the updated lines, the running total and an out-of-band picker that
    /// is empty and focused, and an empty cost falls back to the product's cost
    /// price.
    #[tokio::test]
    async fn n4_purchase_line_scan_adds_in_one_step_and_resets_the_picker() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let scanned = seed_extra_product(&state, "SCAN-PUR", Some("7791234567891")).await;
        let app = crate::routes::router(state.clone());

        // Exactly what the picker form posts on Enter: the typed value and the
        // quantity, no product id and no click.
        let (status, _, added) = post_form_response(
            app,
            &format!("/web/purchases/{}/lines", fixture.purchase_id),
            "product=7791234567891&qty=2&unit_cost=",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{added}");

        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(detail.lines.len(), 2, "the scan adds its own line");
        let line = detail
            .lines
            .iter()
            .find(|line| line.product_id == scanned.id)
            .expect("the scanned line");
        assert_eq!(line.qty, Decimal::from(2));
        assert_eq!(
            line.unit_cost,
            Decimal::from(10),
            "an empty cost falls back to the product cost price"
        );

        // One response carries the lines, the running total and the OOB picker, so
        // lines and total can never drift.
        assert!(added.contains(&scanned.name), "{added:.600}");
        assert!(
            added.contains("$40"),
            "the running total travels with the lines: {added:.800}"
        );
        assert_oob_picker_is_empty_and_focused(&added);
    }

    /// AC10 (clicked result): a result is its own add action; the request includes
    /// the picker form, so the quantity travels, and supplies the product id
    /// itself. The typed text is not an exact match on purpose.
    #[tokio::test]
    async fn n4_purchase_line_clicked_result_uses_the_picker_quantity() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let clicked_product = seed_extra_product(&state, "CLICK-PUR", None).await;
        let app = crate::routes::router(state.clone());

        let (status, _, body) = post_form_response(
            app,
            &format!("/web/purchases/{}/lines", fixture.purchase_id),
            &format!(
                "product=record&qty=3&unit_cost=&product_id={}",
                clicked_product.id
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let detail = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        let clicked = detail
            .lines
            .iter()
            .find(|line| line.product_id == clicked_product.id)
            .expect("the clicked line");
        assert_eq!(clicked.qty, Decimal::from(3));
        assert_eq!(clicked.unit_cost, Decimal::from(10));
        assert_eq!(detail.total, Decimal::from(50));
        assert!(body.contains("$50"), "{body:.800}");
    }

    /// AC12: an unresolvable value is a clear 400 that names the number of partial
    /// matches the search found, and it adds nothing.
    #[tokio::test]
    async fn n4_purchase_line_unknown_value_is_400_and_adds_nothing() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());
        let before = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();

        let (status, _, body) = post_form_response(
            app,
            &format!("/web/purchases/{}/lines", fixture.purchase_id),
            "product=record&qty=1&unit_cost=",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("no exact match"), "{body}");
        assert!(
            body.contains("1 match"),
            "the message must name the search count: {body}"
        );

        let after = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(
            after.lines.len(),
            before.lines.len(),
            "a failed resolution adds no line"
        );
        assert_eq!(after.total, before.total);
    }

    /// The repeated-product rule is a deliberate rejection, not a crash: the route
    /// answers 400 with the actionable message, and the picker form names its action
    /// so the notice region reads "Add line failed — …" instead of a bare error.
    #[tokio::test]
    async fn web_purchase_line_repeated_product_is_a_clear_400() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());
        let before = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();

        let (status, _, body) = post_form_response(
            app.clone(),
            &format!("/web/purchases/{}/lines", fixture.purchase_id),
            &format!("product={}&qty=1&unit_cost=", fixture.product_sku),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("already has a line"), "{body}");
        assert!(
            body.contains("separate purchase"),
            "the message must point at the supported path: {body}"
        );

        let after = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(
            after.lines.len(),
            before.lines.len(),
            "the repeated product adds no line"
        );
        assert_eq!(after.total, before.total);
    }

    #[tokio::test]
    async fn web_create_purchase_then_list_shows_it() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let (st, v) = post_json(
            app.clone(),
            "/api/suppliers",
            serde_json::json!({ "name": "Web Sup" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed supplier: {v}");
        let sup = v["id"].as_i64().unwrap();

        let body = format!("supplier_id={sup}&payment_type=Cash&purchase_date=2024-05-02");
        assert_eq!(
            post_form(app.clone(), "/web/purchases", &body).await,
            StatusCode::OK
        );

        let (status, html) = get_html(app.clone(), "/web/purchases").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("Web Sup"),
            "list should contain the new draft: {html:.400}"
        );
        assert!(
            html.contains("draft #"),
            "draft without number should show as draft #id: {html:.400}"
        );
    }

    #[tokio::test]
    async fn web_seed_from_suggestion_creates_draft_with_line() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let (st, v) = post_json(
            app.clone(),
            "/api/products",
            serde_json::json!({
                "sku": "WEB-SUG", "name": "prod WEB-SUG", "kind": "Product",
                "unit": "un", "sale_price": "10", "cost_price": "5",
                "track_stock": true, "min_stock": "5", "max_stock": "50"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed product: {v}");
        let pid = v["id"].as_i64().unwrap();
        let (st, _) = post_json(
            app.clone(),
            "/api/stock-movements",
            serde_json::json!({
                "product_id": pid, "qty": "2", "type": "In",
                "reason": "Initial", "date": "2024-05-01"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, v) = post_json(
            app.clone(),
            "/api/suppliers",
            serde_json::json!({ "name": "Seed Sup" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed supplier: {v}");
        let sup = v["id"].as_i64().unwrap();
        // A second low-stock product with no satellite row lands in `without_supplier`.
        let (st, _) = post_json(
            app.clone(),
            "/api/products",
            serde_json::json!({
                "sku": "WEB-NOSUP", "name": "prod WEB-NOSUP", "kind": "Product",
                "unit": "un", "sale_price": "10", "cost_price": "5",
                "track_stock": true, "min_stock": "5", "max_stock": "20"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(
            app.clone(),
            "/api/product-supplier-costs",
            serde_json::json!({
                "product_id": pid, "supplier_id": sup, "cost": "7.50", "date": "2024-05-01"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);

        // The fragment endpoint renders the same suggestion the panel shows.
        let (st, html) = get_html(app.clone(), "/web/purchases/suggestions").await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            html.contains("Seed Sup") && html.contains("48") && html.contains("Without supplier cost"),
            "suggestion fragment should render costed and unsourced rows: {html:.400}"
        );

        let body = format!("product_id={pid}&payment_type=Cash&purchase_date=2024-05-02");
        let (status, redirect, resp) =
            post_form_response(app.clone(), "/web/purchases/from-suggestion", &body).await;
        assert_eq!(status, StatusCode::OK, "{resp}");
        let redirect = redirect.expect("seeding a draft must land on its record");
        assert!(redirect.starts_with("/purchases/"), "{redirect}");
        let redirected_id: i64 = redirect["/purchases/".len()..]
            .parse()
            .unwrap_or_else(|_| panic!("HX-Redirect must end in the purchase id: {redirect}"));

        let (status, html) = get_html(app.clone(), "/web/purchases").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("Seed Sup"),
            "seeded draft should appear in the list: {html:.400}"
        );

        // The seeded draft exposes the suggested line in its detail fragment.
        let req = Request::builder()
            .method("GET")
            .uri("/api/purchases")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let purchase_id = v["purchases"][0]["purchase"]["id"].as_i64().unwrap();
        assert_eq!(
            redirected_id, purchase_id,
            "the seed must navigate to the draft it just created"
        );
        let (st, detail) = get_html(app.clone(), &format!("/web/purchases/{purchase_id}")).await;
        assert_eq!(st, StatusCode::OK);
        assert!(
            detail.contains("prod WEB-SUG") && detail.contains("48"),
            "seeded line should show the product name and suggested qty: {detail:.400}"
        );
    }

    // -- S7 enforcement (AC10): the permission gates on the real handlers ------

    /// Like [`get_html`], but with an explicit cookie: `None` means the truly
    /// anonymous request (the shared TEST_COOKIE belongs to the
    /// full-permission principal).
    async fn get_html_as(
        app: axum::Router,
        uri: &str,
        cookie: Option<&str>,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder().method("GET").uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        let req = builder.body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// Like [`post_form`], but with an explicit cookie and optional headers:
    /// an empty `HX-Request` set means the plain browser post the full-page
    /// refusal shape needs. Returns the full body so refusals can be asserted.
    async fn post_form_as(
        app: axum::Router,
        uri: &str,
        body: &str,
        extra_headers: &[(&str, &str)],
        cookie: Option<&str>,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded");
        for (name, value) in extra_headers {
            builder = builder.header(*name, *value);
        }
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        let req = builder.body(Body::from(body.to_string())).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// The read gates are real too: a principal WITHOUT `purchases.read` (it
    /// holds an unrelated permission, so this is not a broken fixture) is
    /// refused every purchases page and fragment with the full-page refusal
    /// card. The suggestions fragment names the stock-derived gate instead.
    #[tokio::test]
    async fn the_read_gates_refuse_a_principal_without_the_read_permission() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["customers.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state);

        for (uri, code) in [
            ("/purchases".to_string(), "purchases.read"),
            (format!("/purchases/{}", fixture.purchase_id), "purchases.read"),
            ("/web/purchases".to_string(), "purchases.read"),
            (
                format!("/web/purchases/{}", fixture.purchase_id),
                "purchases.read",
            ),
            (
                "/web/purchases/suggestions".to_string(),
                "inventory.read",
            ),
        ] {
            let (status, html) = get_html_as(app.clone(), &uri, Some(&cookie)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {html:.200}");
            assert!(
                html.contains("Acción no permitida") && html.contains(code),
                "{uri} must refuse naming {code}: {html:.300}"
            );
        }
    }

    // -- S7 part 2: the page and its fragment can no longer disagree ----------

    /// The old consequence (the S7 part 1 review's UX item): a
    /// `purchases.read`-only principal saw the reorder suggestions
    /// server-rendered into `/purchases` and was refused them on refresh,
    /// because the fragment and the API are gated `inventory.read`. Now the
    /// page renders the Sugerido block only when the principal holds that
    /// same gate: no block that answers 403 on its own refresh button.
    #[tokio::test]
    async fn ac21_the_suggestions_block_hides_from_a_principal_that_cannot_refresh_it() {
        let state = test_state().await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["purchases.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        let (status, html) = get_html_as(app.clone(), "/purchases", Some(&cookie)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            !html.contains("id=\"suggestion-section\""),
            "the Sugerido block must not render for a principal the suggestions \
             fragment would refuse: {html:.600}"
        );
        assert!(!html.contains("Sugerido"), "{html:.600}");

        // The same page for a principal holding BOTH codes: the block is back.
        let holder = test_support::seed_session_with_permissions(
            &state.pool,
            &["purchases.read", "inventory.read"],
        )
        .await
        .unwrap();
        let (status, html) =
            get_html_as(app, "/purchases", Some(&test_support::cookie_for(&holder))).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("id=\"suggestion-section\"") && html.contains("Sugerido"),
            "a principal that may refresh the suggestions must see the block: {html:.600}"
        );
    }

    /// A principal holding ONLY `purchases.read` opens the reads and is
    /// refused every web mutation, each in the shape its caller reads and
    /// naming its own code: the draft lifecycle `purchases.create`, paying
    /// `purchases.create` (the payment is a purchase-side movement), and
    /// cancelling `purchases.cancel`.
    #[tokio::test]
    async fn ac10_a_purchases_read_only_principal_is_refused_the_web_mutations() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["purchases.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        // The reads the probe is allowed: the page, the record and the fragments.
        for uri in [
            "/purchases".to_string(),
            format!("/purchases/{}", fixture.purchase_id),
            "/web/purchases".to_string(),
            format!("/web/purchases/{}", fixture.purchase_id),
        ] {
            let (status, html) = get_html_as(app.clone(), &uri, Some(&cookie)).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {html:.200}");
        }

        // Creating a purchase over HTMX: JSON naming the recording gate.
        let (status, body) = post_form_as(
            app.clone(),
            "/web/purchases",
            &format!("supplier_id={}&payment_type=Cash", fixture.supplier_id),
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(
            body.contains("purchases.create"),
            "the HTMX refusal must name purchases.create: {body}"
        );

        // The same create as a plain browser post: the HTML refusal card.
        let (status, body) = post_form_as(
            app.clone(),
            "/web/purchases",
            &format!("supplier_id={}&payment_type=Cash", fixture.supplier_id),
            &[],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body:.400}");
        assert!(
            body.contains("Acción no permitida") && body.contains("purchases.create"),
            "the refusal must speak Spanish and name the gate: {body:.400}"
        );

        // Seeding from a suggestion creates a purchase: purchases.create.
        let (status, body) = post_form_as(
            app.clone(),
            "/web/purchases/from-suggestion",
            "product_id=1&payment_type=Cash",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(body.contains("purchases.create"), "{body}");

        // The record actions: lines and header are the recording gate.
        for (uri, body) in [
            (
                format!("/web/purchases/{}/lines", fixture.purchase_id),
                format!("product_id={}&qty=1", fixture.product_id),
            ),
            (
                format!("/web/purchases/{}/header", fixture.purchase_id),
                "notes=hacked".to_string(),
            ),
            (
                format!("/web/purchases/{}/confirm", fixture.purchase_id),
                "method_id=".to_string(),
            ),
            (
                format!("/web/purchases/{}/payments", fixture.purchase_id),
                format!("method_id={}&amount=5&date=2024-05-03", fixture.method_id),
            ),
        ] {
            let (status, body) = post_form_as(
                app.clone(),
                &uri,
                &body,
                &[("HX-Request", "true")],
                Some(&cookie),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
            assert!(
                body.contains("purchases.create"),
                "{uri} must name purchases.create: {body}"
            );
        }

        // Line edit and removal, each in its own method (the web routes
        // register POST for the edit, DELETE for the removal).
        for (method, uri, body) in [
            (
                "POST",
                format!(
                    "/web/purchases/{}/lines/{}",
                    fixture.purchase_id, fixture.line_id
                ),
                "qty=9&unit_cost=9",
            ),
            (
                "DELETE",
                format!(
                    "/web/purchases/{}/lines/{}",
                    fixture.purchase_id, fixture.line_id
                ),
                "",
            ),
        ] {
            let req = Request::builder()
                .method(method)
                .uri(&uri)
                .header("content-type", "application/x-www-form-urlencoded")
                .header("HX-Request", "true")
                .header("cookie", &cookie)
                .body(Body::from(body.to_string()))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
            let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(
                text.contains("purchases.create"),
                "{method} {uri} must name purchases.create: {text}"
            );
        }

        // The cancel is its own tier.
        let (status, body) = post_form_as(
            app,
            &format!("/web/purchases/{}/cancel", fixture.purchase_id),
            "reason=",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(body.contains("purchases.cancel"), "{body}");
    }

    /// The old collection endpoints (id in the body) are separate gated
    /// boundaries: the adapter declares its own gate and the delegated call is
    /// a plain function call through the ungated `*_impl` body, so removing
    /// the ADAPTER's gate is exactly what this test pins (the path endpoints
    /// pin the inner handlers above).
    #[tokio::test]
    async fn the_purchases_collection_adapters_carry_their_own_gate() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let probe = test_support::seed_session_with_permissions(
            &state.pool,
            &["purchases.read", "suppliers.read"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state);

        let cases = [
            (
                "/web/purchases/lines",
                format!(
                    "purchase_id={}&product_id={}&qty=1",
                    fixture.purchase_id, fixture.product_id
                ),
                "purchases.create",
            ),
            (
                "/web/purchases/confirm",
                format!("purchase_id={}&method_id=", fixture.purchase_id),
                "purchases.create",
            ),
            (
                "/web/purchases/payments",
                format!(
                    "purchase_id={}&method_id={}&amount=5&date=2024-05-03",
                    fixture.purchase_id, fixture.method_id
                ),
                "purchases.create",
            ),
            (
                "/web/purchases/cancel",
                format!("purchase_id={}&reason=", fixture.purchase_id),
                "purchases.cancel",
            ),
        ];
        for (uri, body, code) in cases {
            let (status, body) = post_form_as(
                app.clone(),
                uri,
                &body,
                &[("HX-Request", "true")],
                Some(&cookie),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
            assert!(body.contains(code), "{uri} must name {code}: {body}");
        }
    }

    /// The refusal writes nothing: the refused creation leaves the purchases
    /// table where it was, the refused confirmation keeps the draft, and the
    /// refused payment writes no payment row.
    #[tokio::test]
    async fn ac10_the_purchases_web_refusal_writes_nothing() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let probe = test_support::seed_session_with_permissions(
            &state.pool,
            &["purchases.read", "suppliers.read"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        let purchases_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM purchases")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let (status, body) = post_form_as(
            app.clone(),
            "/web/purchases",
            &format!("supplier_id={}&payment_type=Cash", fixture.supplier_id),
            &[],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body:.200}");
        let purchases_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM purchases")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(purchases_after, purchases_before, "a refused create must write nothing");

        let (status, body) = post_form_as(
            app.clone(),
            &format!("/web/purchases/{}/confirm", fixture.purchase_id),
            "method_id=",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let after = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap();
        assert_eq!(
            after.purchase.status,
            crate::models::PurchaseStatus::Draft,
            "a refused confirm must not flip the status"
        );

        state
            .purchases_service
            .confirm(audit_actor(&state).await, fixture.purchase_id, None)
            .await
            .unwrap();
        let payments_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM purchase_payments")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let (status, body) = post_form_as(
            app,
            &format!("/web/purchases/{}/payments", fixture.purchase_id),
            &format!("method_id={}&amount=5&date=2024-05-03", fixture.method_id),
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let payments_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM purchase_payments")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(payments_after, payments_before, "a refused payment must write nothing");
    }

    /// A principal holding the permissions gets the normal answers: the
    /// creation redirects to the new record, the seeded suggestion creates a
    /// draft, and the record actions answer their fragments.
    #[tokio::test]
    async fn ac10_the_purchases_web_holding_principal_gets_the_normal_answer() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let holder = test_support::seed_session_with_permissions(
            &state.pool,
            &[
                "purchases.read",
                "purchases.create",
                "purchases.cancel",
                "suppliers.read",
                "purchases.costs.read",
                "inventory.read",
            ],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&holder);
        let app = crate::routes::router(state.clone());

        // Create: the plain form path redirects to the record.
        let (status, body) = post_form_as(
            app.clone(),
            "/web/purchases",
            &format!(
                "supplier_id={}&payment_type=Cash&purchase_date=2024-05-02",
                fixture.supplier_id
            ),
            &[],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body:.200}");

        // Confirm, payment and cancel: their normal HTMX fragments.
        let (status, body) = post_form_as(
            app.clone(),
            &format!("/web/purchases/{}/confirm", fixture.purchase_id),
            "method_id=",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.300}");

        let (status, body) = post_form_as(
            app.clone(),
            &format!("/web/purchases/{}/payments", fixture.purchase_id),
            &format!("method_id={}&amount=10&date=2024-05-03", fixture.method_id),
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.300}");

        let (status, _body) = post_form_as(
            app,
            &format!("/web/purchases/{}/cancel", fixture.purchase_id),
            "reason=wrong order",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    /// The gate order must not change: an anonymous request gets the login
    /// redirect, never the permission refusal.
    #[tokio::test]
    async fn an_anonymous_request_still_gets_the_login_gate_not_the_permission_refusal() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let (status, _) = get_html_as(app.clone(), "/purchases", None).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let (status, _) = get_html_as(app, "/web/purchases", None).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
    }

    // -- DELETE /web/purchases/{id}: the documents drawer's draft delete -------

    /// DELETE carries no body; the interesting answer is the status plus the
    /// `HX-Trigger` the documents page listens for.
    async fn send_delete(
        app: axum::Router,
        uri: &str,
        cookie: Option<&str>,
    ) -> (StatusCode, String, Option<String>) {
        let mut builder = Request::builder().method("DELETE").uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        let req = builder.body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let trigger = resp
            .headers()
            .get("HX-Trigger")
            .map(|v| v.to_str().unwrap().to_string());
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string(), trigger)
    }

    #[tokio::test]
    async fn web_delete_draft_purchase_answers_200_with_the_purchase_changed_trigger() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());

        let (status, body, trigger) = send_delete(
            app,
            &format!("/web/purchases/{}", fixture.purchase_id),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            trigger.as_deref(),
            Some("purchase-changed"),
            "the documents page listens for purchase-changed"
        );
        let err = state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .unwrap_err();
        assert!(
            matches!(err, crate::error::AppError::NotFound(_)),
            "the deleted draft must be gone: {err:?}"
        );
    }

    #[tokio::test]
    async fn web_delete_draft_refuses_a_confirmed_purchase_with_400() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .purchases_service
            .confirm(
                audit_actor(&state).await,
                fixture.purchase_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, body, _) = send_delete(
            app,
            &format!("/web/purchases/{}", fixture.purchase_id),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn web_delete_draft_requires_purchases_create() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());
        let probe = test_support::seed_session_with_permissions(&state.pool, &["purchases.read"])
            .await
            .unwrap();

        let (status, _, _) = send_delete(
            app,
            &format!("/web/purchases/{}", fixture.purchase_id),
            Some(&test_support::cookie_for(&probe)),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(state
            .purchases_service
            .get_detail(fixture.purchase_id)
            .await
            .is_ok());
    }
}
