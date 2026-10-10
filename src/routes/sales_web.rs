// Sales web: the `/sales` list and the `/sales/{id}` record page, Askama + HTMX.
// Thin handlers over SalesService; the record body lives in
// partials/sale_detail.html and every action posts to `/web/sales/{id}/...`,
// so the id always comes from the URL. The old collection endpoints (id in the
// form body) stay registered for existing callers.
use askama::Template;
use axum::{
    extract::{Extension, Form, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::localization::LocalizationContext;
use crate::models::{
    DebtSummary, PaymentType, SaleListFilter, SaleRecord, SaleStatus, UpdateSaleDraft,
};
use crate::routes::{localized_refusal_error, AppState};
use crate::security::authz::{CustomersCollect, Nav, Require, SalesCancel, SalesCreate, SalesRead};
use crate::services::sales::DEBT_BANNER_LIMIT;

// S6 enforcement (AC10): reads are `sales.read`; the draft lifecycle (create,
// edit, lines, confirm) is `sales.create` — the catalog has no `sales.write`,
// so recording a sale IS editing the draft; cancelling is its own tier
// `sales.cancel`; money received on a confirmed sale is the collection
// capability `customers.collect`, not `sales.create` (see `web_record_payment`).

// ---------------------------------------------------------------------------
// Askama templates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "sales.html")]
struct SalesTemplate {
    title: String,
    localization: LocalizationContext,
    /// The documents, each with its refusal already resolved: a row whose
    /// arithmetic could not be carried renders in place with no figure.
    sales: Vec<SaleRowView>,
    /// How many documents are unpaid — a fact even when the total is a refusal.
    debt_count: usize,
    /// The banner's headline: the amount, or the refusal in the operator's
    /// language. A sum over every unpaid document, so never partial.
    debt_total: String,
    /// True when `debt_total` is a real figure, so the panel shows it as one.
    debt_totalled: bool,
    /// The banner's documents, with their own refusals resolved.
    debt_oldest: Vec<SaleRowView>,
    customers: Vec<crate::models::Customer>,
    today: String,
    nav_key: &'static str,
    /// Current filter values, so a bookmarkable `/sales?status=…` re-renders with
    /// the same form state the server used for the list.
    filter_status: String,
    filter_customer: String,
    filter_number: String,
    filter_from: String,
    filter_to: String,
    /// The sidebar's nav view: the entries this principal may read (S7 part 2).
    nav: Nav,
}

/// The `/sales/{id}` record page. The page-header values are struct fields, so
/// the shared component and the record body read them straight from the shell.
#[derive(Template)]
#[template(path = "sale.html")]
struct SalePageTemplate {
    localization: LocalizationContext,
    /// Sale number, or "Draft sale" before confirmation.
    page_title: String,
    page_breadcrumb_label: String,
    page_breadcrumb_href: String,
    /// Empty label = no primary action (a cancelled sale is read-only).
    page_action_href: String,
    page_action_label: String,
    record: SaleRecord,
    oob_picker: bool,
    method_options: Vec<crate::models::PaymentMethodWithAccount>,
    today: String,
    /// Audit display names (M5 Phase B, slice S11): the sale's creator and its
    /// last editor, resolved in this wiring layer.
    created_by_name: Option<String>,
    updated_by_name: Option<String>,
    /// The document-total refusal in the operator's language, empty when the
    /// document totals exactly. The page includes the same record partial the
    /// action responses render, so the sentence travels with it.
    total_refusal_message: String,
    /// Whether the acting principal holds `sales.create`. See
    /// `SaleRecordContext::can_credit`.
    can_credit: bool,
    nav_key: &'static str,
    /// The sidebar's nav view: the entries this principal may read (S7 part 2).
    nav: Nav,
}

/// One row of a document list, as a page needs it: the document's own fields,
/// flattened, plus the refusal already in the operator's language.
///
/// The sentence is resolved HERE, through the one shared `price_refusal_key`
/// mapping, so no surface can word the rule differently from the record pages,
/// the customer drawer or the index. `money` is `None` exactly when the message
/// is not empty: the two travel together so the template has one thing to render
/// in place of the figure.
struct SaleRowView {
    sale: crate::models::Sale,
    money: Option<crate::models::RecordMoney>,
    total_refusal_message: String,
}

fn sale_row_views(
    rows: Vec<crate::models::SaleListRow>,
    localization: &LocalizationContext,
) -> Vec<SaleRowView> {
    rows.into_iter()
        .map(|row| SaleRowView {
            sale: row.sale,
            money: row.money,
            total_refusal_message: row
                .total_refusal
                .map(|refusal| crate::routes::price_refusal_message(&refusal, localization))
                .unwrap_or_default(),
        })
        .collect()
}

#[derive(Template)]
#[template(path = "partials/sale_list.html")]
struct SaleListPartial {
    title: String,
    localization: LocalizationContext,
    sales: Vec<SaleRowView>,
}

/// The debt banner: total owed, unpaid count and the oldest few. A summary, so
/// the always-rendered panel never loads the whole receivable history.
#[derive(Template)]
#[template(path = "partials/sale_debt.html")]
struct SaleDebtPartial {
    localization: LocalizationContext,
    /// How many documents are unpaid. The panel is a summary, so the COUNT is a
    /// fact even when the total is a refusal: the operator still needs to know how
    /// many documents are behind.
    debt_count: usize,
    /// The banner's headline, already resolved: the amount, or the refusal in the
    /// operator's language. A sum over every unpaid document in the shop, so it
    /// is refused rather than partial.
    debt_total: String,
    /// True when `debt_total` is a real figure, so the panel shows it as one.
    debt_totalled: bool,
    /// The banner's documents, with their own refusals resolved.
    debt_oldest: Vec<SaleRowView>,
}

/// The record body, shared by the page and by every action response that swaps
/// `#sale-record`, so the action forms travel with the fragment either way.
#[derive(Template)]
#[template(path = "partials/sale_detail.html")]
struct SaleDetailPartial {
    record: SaleRecord,
    localization: LocalizationContext,
    oob_picker: bool,
    method_options: Vec<crate::models::PaymentMethodWithAccount>,
    today: String,
    /// Audit display names: who created the sale, and who last edited it.
    created_by_name: Option<String>,
    updated_by_name: Option<String>,
    /// The document-total refusal, already in the operator's language, or empty
    /// when the document totals exactly. Resolved HERE, through the one shared
    /// `price_refusal_key` mapping, so the record page cannot word the rule
    /// differently from the add-line response or the index.
    total_refusal_message: String,
    /// Whether the acting principal holds `sales.create`, which is the gate the
    /// record body's "Issue credit note" action posts into. See
    /// `SaleRecordContext::can_credit`.
    can_credit: bool,
}

// ---------------------------------------------------------------------------
// Helpers (mirror inventory_web)
// ---------------------------------------------------------------------------

fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .map(|v| v == "true")
        .unwrap_or(false)
}

fn parse_required_decimal(
    s: &str,
    field: &str,
    localization: &LocalizationContext,
) -> AppResult<Decimal> {
    localization
        .parse_decimal(s.trim())
        .map_err(|_| AppError::Validation(format!("invalid {field}")))
}

fn parse_opt_decimal(
    s: &str,
    field: &str,
    localization: &LocalizationContext,
) -> AppResult<Option<Decimal>> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    localization
        .parse_decimal(t)
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

/// Empty means "not provided", which for the header edit clears the value and
/// lets the service resolve defaults (for example a credit due date).
fn parse_opt_date_field(s: &str, field: &str) -> AppResult<Option<NaiveDate>> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse()
        .map(Some)
        .map_err(|_| AppError::Validation(format!("invalid {field} (YYYY-MM-DD)")))
}

fn parse_date_or_today(s: &str, localization: &LocalizationContext) -> AppResult<NaiveDate> {
    let t = s.trim();
    if t.is_empty() {
        return localization
            .today_iso()
            .parse()
            .map_err(|_| AppError::Internal("invalid localized date".into()));
    }
    t.parse()
        .map_err(|_| AppError::Validation("invalid date (YYYY-MM-DD)".into()))
}

fn render_list(
    sales: Vec<SaleRowView>,
    title: &str,
    localization: LocalizationContext,
) -> AppResult<Html<String>> {
    let html = SaleListPartial {
        title: title.to_string(),
        localization,
        sales,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

fn render_debt(debt: DebtSummary, localization: LocalizationContext) -> AppResult<Html<String>> {
    let (debt_total, debt_totalled) = match debt.total.amount {
        Some(amount) => (localization.format_currency(amount), true),
        None => (
            debt.total
                .refusal
                .map(|refusal| crate::routes::price_refusal_message(&refusal, &localization))
                .unwrap_or_default(),
            false,
        ),
    };
    let debt_count = debt.count;
    let debt_oldest = sale_row_views(debt.oldest, &localization);
    let html = SaleDebtPartial {
        localization,
        debt_count,
        debt_total,
        debt_totalled,
        debt_oldest,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

/// Everything the record body renders: the resolved record plus the
/// method-with-account options its action forms need. The product picker
/// searches `/web/product-search.json` instead of carrying the whole catalogue.
struct SaleRecordContext {
    record: SaleRecord,
    localization: LocalizationContext,
    method_options: Vec<crate::models::PaymentMethodWithAccount>,
    today: String,
    /// Audit display names: the sale's creator and its last editor, resolved
    /// here in the wiring layer (AC20: the service never reads identity).
    created_by_name: Option<String>,
    updated_by_name: Option<String>,
    /// The document-total refusal in the operator's language, empty when the
    /// document totals exactly. See [`SaleRecordContext::record`] for why the
    /// page states it instead of answering an error.
    total_refusal_message: String,
    /// Whether this principal holds `sales.create` — the gate
    /// `POST /web/customer-returns` declares. The "Issue credit note" action
    /// renders only when true, so the record page never offers an action the
    /// route would refuse. Resolved in the wiring layer from the request's own
    /// principal, on EVERY render path including the fragment.
    can_credit: bool,
}

async fn record_context(
    state: &AppState,
    sale_id: i64,
    localization: LocalizationContext,
    can_credit: bool,
) -> AppResult<SaleRecordContext> {
    let record = state.sales_service.get_record(sale_id).await?;
    let method_options = state.payment_method_service.methods_with_accounts().await?;
    let today = localization.today_iso();
    let mut actor_ids = vec![record.sale.created_by];
    actor_ids.extend(record.sale.updated_by);
    let names = crate::routes::audit_actor_names(&state.pool, &actor_ids).await?;
    let name_for = |id: i64| names.get(&id).cloned();
    let created_by_name = name_for(record.sale.created_by);
    let updated_by_name = record.sale.updated_by.and_then(name_for);
    let total_refusal_message = record
        .total_refusal
        .map(|refusal| crate::routes::price_refusal_message(&refusal, &localization))
        .unwrap_or_default();
    Ok(SaleRecordContext {
        record,
        localization,
        method_options,
        today,
        created_by_name,
        updated_by_name,
        total_refusal_message,
        can_credit,
    })
}

fn render_record(context: SaleRecordContext, oob_picker: bool) -> AppResult<Html<String>> {
    let html = SaleDetailPartial {
        record: context.record,
        localization: context.localization,
        oob_picker,
        method_options: context.method_options,
        today: context.today,
        created_by_name: context.created_by_name,
        updated_by_name: context.updated_by_name,
        total_refusal_message: context.total_refusal_message,
        can_credit: context.can_credit,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

/// Record-body response that keeps the cross-region `sale-changed` refresh
/// event, so subscribed list and debt regions update after an action.
async fn changed(
    state: &AppState,
    sale_id: i64,
    localization: &LocalizationContext,
    can_credit: bool,
) -> AppResult<Response> {
    changed_with_picker(state, sale_id, localization, can_credit, false).await
}

/// Line-add response: the same body plus the out-of-band picker, empty and
/// focused, so the scanner can feed the next line without a click.
async fn changed_with_picker(
    state: &AppState,
    sale_id: i64,
    localization: &LocalizationContext,
    can_credit: bool,
    oob_picker: bool,
) -> AppResult<Response> {
    let html = render_record(
        record_context(state, sale_id, localization.clone(), can_credit).await?,
        oob_picker,
    )?
    .0;
    let mut resp = Html(html).into_response();
    resp.headers_mut()
        .insert("HX-Trigger", "sale-changed".parse().unwrap());
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Page + fragments
// ---------------------------------------------------------------------------

async fn sales_page(
    State(state): State<AppState>,
    _: Require<SalesRead>,
    Extension(localization): Extension<LocalizationContext>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Query(query): Query<SaleListQuery>,
) -> Result<Html<String>, AppError> {
    // A list of documents can carry one whose total cannot be computed, and
    // this page is where the operator goes to FIND it. It refuses the whole
    // page rather than dropping that document silently — a list that quietly
    // omits a stored sale is a lie about the shop's history — and it refuses
    // in the operator's language, through the one shared mapping.
    // The ROW read, not the detail read: a list must render a document whose
    // total cannot be computed instead of answering an error and taking every
    // other document on the page with it.
    let sales = sale_row_views(
        state
            .sales_service
            .list_rows_filtered(&query.to_filter())
            .await?,
        &localization,
    );
    let debt = state.sales_service.debt_summary(DEBT_BANNER_LIMIT).await?;
    let customers = state.customer_service.list_customers(true).await?;
    let today = localization.today_iso();
    let title = localization
        .tr(crate::localization::MessageKey::SalesAll)
        .to_string();
    let (debt_total, debt_totalled) = match debt.total.amount {
        Some(amount) => (localization.format_currency(amount), true),
        None => (
            debt.total
                .refusal
                .map(|refusal| crate::routes::price_refusal_message(&refusal, &localization))
                .unwrap_or_default(),
            false,
        ),
    };
    let debt_count = debt.count;
    let debt_oldest = sale_row_views(debt.oldest, &localization);
    let tmpl = SalesTemplate {
        title,
        localization,
        sales,
        debt_count,
        debt_total,
        debt_totalled,
        debt_oldest,
        customers,
        today,
        nav_key: "sales",
        filter_status: query.status.trim().to_string(),
        filter_customer: query.customer.trim().to_string(),
        filter_number: query.number.trim().to_string(),
        filter_from: query.from.trim().to_string(),
        filter_to: query.to.trim().to_string(),
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(
        tmpl.render()
            .map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

/// Query parameters for the sales list filter. Every field is optional and an
/// empty or unparseable value is treated as absent, so a filterless or partial URL
/// is never an error. The document number matches partially, because a user
/// remembers a fragment of it.
#[derive(Debug, Deserialize, Default)]
pub struct SaleListQuery {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub customer: String,
    #[serde(default)]
    pub number: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
}

impl SaleListQuery {
    fn to_filter(&self) -> SaleListFilter {
        SaleListFilter {
            status: parse_optional_status(&self.status),
            customer: clean_filter_text(&self.customer),
            customer_ids: None,
            number: clean_filter_text(&self.number),
            from: parse_optional_date(&self.from),
            to: parse_optional_date(&self.to),
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

fn parse_optional_status(raw: &str) -> Option<SaleStatus> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse().ok()
}

fn parse_optional_date(raw: &str) -> Option<NaiveDate> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse().ok()
}

async fn web_sale_list(
    State(state): State<AppState>,
    _: Require<SalesRead>,
    Extension(localization): Extension<LocalizationContext>,
    Query(query): Query<SaleListQuery>,
) -> Result<Html<String>, AppError> {
    // The ROW read, the same reason and the same mapping as the page above.
    let sales = sale_row_views(
        state
            .sales_service
            .list_rows_filtered(&query.to_filter())
            .await?,
        &localization,
    );
    let title = localization
        .tr(crate::localization::MessageKey::SalesAll)
        .to_string();
    render_list(sales, &title, localization)
}

async fn web_sale_debt(
    State(state): State<AppState>,
    _: Require<SalesRead>,
    Extension(localization): Extension<LocalizationContext>,
) -> Result<Html<String>, AppError> {
    let debt = state.sales_service.debt_summary(DEBT_BANNER_LIMIT).await?;
    render_debt(debt, localization)
}

/// `/sales/{id}`: a real page inside the shell. The label is the sale number or
/// its draft state, and the single header action slot mirrors the status.
async fn sale_record_page(
    State(state): State<AppState>,
    _: Require<SalesRead>,
    Extension(localization): Extension<LocalizationContext>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
) -> Result<Html<String>, AppError> {
    let context = record_context(
        &state,
        id,
        localization.clone(),
        principal.has_permission::<SalesCreate>(),
    )
    .await?;
    let label = match &context.record.sale.sale_number {
        Some(number) => number.clone(),
        None => context
            .localization
            .tr(crate::localization::MessageKey::SalesDraft)
            .to_string(),
    };
    let (action_href, action_label) = if context.record.sale.status == SaleStatus::Draft {
        (
            "#add-line".to_string(),
            context
                .localization
                .tr(crate::localization::MessageKey::SalesAddLine)
                .to_string(),
        )
    } else if context.record.sale.status == SaleStatus::Confirmed
        && context.record.sale.payment_type == PaymentType::Credit
    {
        (
            "#record-payment".to_string(),
            context
                .localization
                .tr(crate::localization::MessageKey::SalesRecordPayment)
                .to_string(),
        )
    } else {
        (String::new(), String::new())
    };
    let tmpl = SalePageTemplate {
        localization: context.localization.clone(),
        page_title: label,
        page_breadcrumb_label: context
            .localization
            .tr(crate::localization::MessageKey::NavigationSales)
            .to_string(),
        page_breadcrumb_href: "/sales".to_string(),
        page_action_href: action_href,
        page_action_label: action_label,
        record: context.record,
        oob_picker: false,
        method_options: context.method_options,
        today: context.today,
        created_by_name: context.created_by_name,
        updated_by_name: context.updated_by_name,
        total_refusal_message: context.total_refusal_message,
        can_credit: context.can_credit,
        nav_key: "sales",
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(
        tmpl.render()
            .map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

async fn web_sale_detail(
    State(state): State<AppState>,
    _: Require<SalesRead>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
) -> Result<Html<String>, AppError> {
    render_record(
        record_context(
            &state,
            id,
            localization,
            principal.has_permission::<SalesCreate>(),
        )
        .await?,
        false,
    )
}

/// `DELETE /web/sales/{id}`: the documents drawer's draft delete — the
/// mirror of the purchase flow, plus the discarded (never-confirmed)
/// cancelled sale the service now admits. The same house shape as the other
/// HTMX writes (`web_delete_transaction`): an empty 200 whose `HX-Trigger`
/// tells the listening pages to re-read the feed — the business outcome
/// lives in the service, the route only answers.
async fn web_delete_draft(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    Path(id): Path<i64>,
) -> Result<axum::response::Response, AppError> {
    state.sales_service.delete_draft(id).await?;
    let mut resp = Html("".to_string()).into_response();
    resp.headers_mut()
        .insert("HX-Trigger", "sale-changed".parse().unwrap());
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Forms (HTMX, mirror products page patterns)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateSaleForm {
    #[serde(default)]
    pub customer_id: Option<i64>,
    #[serde(default)]
    pub payment_type: String,
    #[serde(default)]
    pub sale_date: String,
    #[serde(default)]
    pub due_date: String,
    #[serde(default)]
    pub receipt_no: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize)]
pub struct AddLineForm {
    #[serde(default)]
    pub sale_id: i64,
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
    pub unit_price: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateLineForm {
    #[serde(default)]
    pub qty: String,
    #[serde(default)]
    pub unit_price: String,
}

#[derive(Debug, Deserialize)]
pub struct ConfirmSaleForm {
    #[serde(default)]
    pub sale_id: i64,
    #[serde(default)]
    pub method_id: String,
}

#[derive(Debug, Deserialize)]
pub struct RecordPaymentForm {
    #[serde(default)]
    pub sale_id: i64,
    pub method_id: i64,
    #[serde(default)]
    pub amount: String,
    #[serde(default)]
    pub date: String,
}

#[derive(Debug, Deserialize)]
pub struct CancelSaleForm {
    #[serde(default)]
    pub sale_id: i64,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateSaleHeaderForm {
    #[serde(default)]
    pub sale_date: String,
    #[serde(default)]
    pub due_date: String,
    #[serde(default)]
    pub receipt_no: String,
    #[serde(default)]
    pub notes: String,
}

async fn web_create_sale(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<CreateSaleForm>,
) -> Result<axum::response::Response, AppError> {
    let payment_type: PaymentType = if form.payment_type.trim().is_empty() {
        PaymentType::Cash
    } else {
        form.payment_type.parse().map_err(AppError::Validation)?
    };
    let receipt_no = if form.receipt_no.trim().is_empty() {
        None
    } else {
        Some(form.receipt_no.trim().to_string())
    };
    let due_date = if form.due_date.trim().is_empty() {
        None
    } else {
        Some(
            form.due_date
                .trim()
                .parse()
                .map_err(|_| AppError::Validation("invalid due_date (YYYY-MM-DD)".into()))?,
        )
    };
    let customer_id = form
        .customer_id
        .ok_or_else(|| AppError::Validation("customer is required".into()))?;
    let sale = state
        .sales_service
        .create_draft(
            principal.user_id,
            crate::models::NewSale {
                customer_id,
                payment_type,
                sale_date: parse_date_or_today(&form.sale_date, &localization)?,
                due_date,
                receipt_no,
                notes: Some(form.notes),
            },
        )
        .await?;
    let location = format!("/sales/{}", sale.id);
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

// Both registered handlers — the path endpoint and the collection adapter —
// declare their own real `Require<P>` and call the shared ungated impl below,
// so removing either gate compiles and fails exactly the test that pins it.
async fn web_add_line(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    Form(form): Form<AddLineForm>,
) -> Result<axum::response::Response, AppError> {
    add_line_impl(
        state,
        principal.user_id,
        principal.has_permission::<SalesCreate>(),
        headers,
        localization,
        id,
        form,
    )
    .await
}

/// `actor` is the authenticated principal's user id, carried here rather than
/// read from `state`: both registered handlers (the path endpoint and the
/// collection adapter) already hold the `Principal` extension, so this is the
/// same `Extension` the `Require<SalesCreate>` gate was resolved from — not a
/// second identity source, and not a fresh actor minted per call.
async fn add_line_impl(
    state: AppState,
    actor: i64,
    can_credit: bool,
    headers: HeaderMap,
    localization: LocalizationContext,
    id: i64,
    form: AddLineForm,
) -> Result<axum::response::Response, AppError> {
    let qty = parse_required_decimal(&form.qty, "qty", &localization)?;
    let unit_price = parse_opt_decimal(&form.unit_price, "unit_price", &localization)?;
    // An explicit product id (a clicked result) wins over the typed text; a scan
    // or an Enter carries only the value and resolves through inventory.
    let product_id = match form.product_id.filter(|id| *id > 0) {
        Some(id) => id,
        None => {
            state
                .inventory_service
                .resolve_product_ref(&form.product)
                .await?
                .id
        }
    };
    state
        .sales_service
        .add_line(actor, id, product_id, qty, unit_price)
        .await
        // A line write runs the shared tax contract, which now REFUSES an
        // amount or a tax arithmetic it cannot carry instead of panicking. The
        // refusal is carried out as a typed rule, so it answers through the same
        // renderer the product form and the ladder use, in the operator's own
        // language. `localized_refusal_error` passes every other error through
        // untouched, so nothing else on this route changes.
        .map_err(|error| localized_refusal_error(error, &localization))?;
    if is_htmx(&headers) {
        return changed_with_picker(&state, id, &localization, can_credit, true).await;
    }
    Ok(Redirect::to(&format!("/sales/{id}")).into_response())
}

async fn web_update_line(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path((sale_id, line_id)): Path<(i64, i64)>,
    Form(form): Form<UpdateLineForm>,
) -> Result<axum::response::Response, AppError> {
    let qty = parse_required_decimal(&form.qty, "qty", &localization)?;
    let unit_price = parse_required_decimal(&form.unit_price, "unit_price", &localization)?;
    state
        .sales_service
        .update_line(principal.user_id, line_id, qty, unit_price)
        .await
        // Same contract, same renderer, same reason as the add: an inline edit
        // of a draft line can carry an unrepresentable amount just as an add can.
        .map_err(|error| localized_refusal_error(error, &localization))?;
    if is_htmx(&headers) {
        return changed(
            &state,
            sale_id,
            &localization,
            principal.has_permission::<SalesCreate>(),
        )
        .await;
    }
    Ok(Redirect::to(&format!("/sales/{sale_id}")).into_response())
}

async fn web_remove_line(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    Path((sale_id, line_id)): Path<(i64, i64)>,
) -> Result<axum::response::Response, AppError> {
    state
        .sales_service
        .remove_line(principal.user_id, line_id)
        .await?;
    changed(
        &state,
        sale_id,
        &localization,
        principal.has_permission::<SalesCreate>(),
    )
    .await
}

async fn web_confirm_sale(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    Form(form): Form<ConfirmSaleForm>,
) -> Result<axum::response::Response, AppError> {
    confirm_sale_impl(
        state,
        principal.user_id,
        principal.has_permission::<SalesCreate>(),
        headers,
        localization,
        id,
        form,
    )
    .await
}

async fn confirm_sale_impl(
    state: AppState,
    actor: i64,
    can_credit: bool,
    headers: HeaderMap,
    localization: LocalizationContext,
    id: i64,
    form: ConfirmSaleForm,
) -> Result<axum::response::Response, AppError> {
    let method_id = parse_opt_i64(&form.method_id, "method_id")?;
    // A refusal reaches the operator in their language: a confirmation can be
    // refused by the document-total rule, and a raw `PriceRefused` would answer
    // with the English wire text instead of the sentence this screen is written
    // in. Every other error travels untouched.
    state
        .sales_service
        .confirm(actor, id, method_id)
        .await
        .map_err(|error| localized_refusal_error(error, &localization))?;
    if is_htmx(&headers) {
        return changed(&state, id, &localization, can_credit).await;
    }
    Ok(Redirect::to(&format!("/sales/{id}")).into_response())
}

// The drawer payment form and its `/api` twin share the capability: money
// received against an owed balance is a collection — `customers.collect` —
// not `sales.create`. See the note on `record_payment` in `sales_api.rs`.
async fn web_record_payment(
    State(state): State<AppState>,
    _: Require<CustomersCollect>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    Form(form): Form<RecordPaymentForm>,
) -> Result<axum::response::Response, AppError> {
    record_payment_impl(
        state,
        principal.user_id,
        principal.has_permission::<SalesCreate>(),
        headers,
        localization,
        id,
        form,
    )
    .await
}

async fn record_payment_impl(
    state: AppState,
    actor: i64,
    can_credit: bool,
    headers: HeaderMap,
    localization: LocalizationContext,
    id: i64,
    form: RecordPaymentForm,
) -> Result<axum::response::Response, AppError> {
    let amount = parse_required_decimal(&form.amount, "amount", &localization)?;
    let date = parse_date_or_today(&form.date, &localization)?;
    // The payment ceiling is measured against the document total, so this path
    // can answer the document-total refusal too — in the operator's language.
    state
        .sales_service
        .record_payment(actor, id, form.method_id, amount, date)
        .await
        .map_err(|error| localized_refusal_error(error, &localization))?;
    if is_htmx(&headers) {
        return changed(&state, id, &localization, can_credit).await;
    }
    Ok(Redirect::to(&format!("/sales/{id}")).into_response())
}

async fn web_cancel_sale(
    State(state): State<AppState>,
    _: Require<SalesCancel>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    Form(form): Form<CancelSaleForm>,
) -> Result<axum::response::Response, AppError> {
    cancel_sale_impl(
        state,
        principal.user_id,
        principal.has_permission::<SalesCreate>(),
        headers,
        localization,
        id,
        form,
    )
    .await
}

async fn cancel_sale_impl(
    state: AppState,
    actor: i64,
    can_credit: bool,
    headers: HeaderMap,
    localization: LocalizationContext,
    id: i64,
    form: CancelSaleForm,
) -> Result<axum::response::Response, AppError> {
    let reason = if form.reason.trim().is_empty() {
        None
    } else {
        Some(form.reason.trim().to_string())
    };
    // The annulment measures refunds against the document's payments, so it can
    // answer the document-total refusal too — in the operator's language.
    state
        .sales_service
        .cancel(actor, id, reason)
        .await
        .map_err(|error| localized_refusal_error(error, &localization))?;
    if is_htmx(&headers) {
        return changed(&state, id, &localization, can_credit).await;
    }
    Ok(Redirect::to(&format!("/sales/{id}")).into_response())
}

/// Edit the draft header in place (dates, receipt, notes); the customer and the
/// payment type stay fixed at creation, as the service enforces.
async fn web_update_sale_header(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    Form(form): Form<UpdateSaleHeaderForm>,
) -> Result<axum::response::Response, AppError> {
    let sale_date = parse_opt_date_field(&form.sale_date, "sale_date")?;
    let due_date = parse_opt_date_field(&form.due_date, "due_date")?;
    let receipt_no = if form.receipt_no.trim().is_empty() {
        None
    } else {
        Some(form.receipt_no.trim().to_string())
    };
    state
        .sales_service
        .update_draft(
            id,
            principal.user_id,
            UpdateSaleDraft {
                sale_date,
                due_date: Some(due_date),
                receipt_no: Some(receipt_no),
                notes: Some(form.notes),
            },
        )
        .await?;
    if is_htmx(&headers) {
        return changed(
            &state,
            id,
            &localization,
            principal.has_permission::<SalesCreate>(),
        )
        .await;
    }
    Ok(Redirect::to(&format!("/sales/{id}")).into_response())
}

// ---------------------------------------------------------------------------
// Collection endpoints (typed id in the form body)
// ---------------------------------------------------------------------------

// HTMX posts the literal `hx-post` URL and never reads the form `action`
// property, so typed-id forms cannot interpolate a path segment. These
// adapters take the sale id from the submitted body and share the ungated
// `*_impl` bodies with the path-based handlers above, keeping both URL shapes
// working (mirrors `purchases_web`).

// Both registered handlers — the path endpoint and the adapter — declare
// their own real `Require<P>` and call the shared ungated impl, so removing
// either gate compiles and fails exactly the test that pins it.
async fn web_add_line_collection(
    state: State<AppState>,
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Form(form): Form<AddLineForm>,
) -> Result<axum::response::Response, AppError> {
    add_line_impl(
        state.0,
        principal.user_id,
        principal.has_permission::<SalesCreate>(),
        headers,
        localization,
        form.sale_id,
        form,
    )
    .await
}

async fn web_confirm_sale_collection(
    state: State<AppState>,
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Form(form): Form<ConfirmSaleForm>,
) -> Result<axum::response::Response, AppError> {
    confirm_sale_impl(
        state.0,
        principal.user_id,
        principal.has_permission::<SalesCreate>(),
        headers,
        localization,
        form.sale_id,
        form,
    )
    .await
}

async fn web_record_payment_collection(
    state: State<AppState>,
    _: Require<CustomersCollect>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Form(form): Form<RecordPaymentForm>,
) -> Result<axum::response::Response, AppError> {
    record_payment_impl(
        state.0,
        principal.user_id,
        principal.has_permission::<SalesCreate>(),
        headers,
        localization,
        form.sale_id,
        form,
    )
    .await
}

async fn web_cancel_sale_collection(
    state: State<AppState>,
    _: Require<SalesCancel>,
    principal: axum::Extension<crate::security::authz::Principal>,
    headers: HeaderMap,
    Extension(localization): Extension<LocalizationContext>,
    Form(form): Form<CancelSaleForm>,
) -> Result<axum::response::Response, AppError> {
    cancel_sale_impl(
        state.0,
        principal.user_id,
        principal.has_permission::<SalesCreate>(),
        headers,
        localization,
        form.sale_id,
        form,
    )
    .await
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/sales", get(sales_page))
        .route("/sales/{id}", get(sale_record_page))
        .route("/web/sales", get(web_sale_list).post(web_create_sale))
        .route("/web/sales/debt", get(web_sale_debt))
        .route("/web/sales/lines", post(web_add_line_collection))
        .route("/web/sales/confirm", post(web_confirm_sale_collection))
        .route("/web/sales/payments", post(web_record_payment_collection))
        .route("/web/sales/cancel", post(web_cancel_sale_collection))
        .route(
            "/web/sales/{id}",
            get(web_sale_detail).delete(web_delete_draft),
        )
        .route("/web/sales/{id}/lines", post(web_add_line))
        .route(
            "/web/sales/{sale_id}/lines/{line_id}",
            post(web_update_line).delete(web_remove_line),
        )
        .route("/web/sales/{id}/header", post(web_update_sale_header))
        .route("/web/sales/{id}/confirm", post(web_confirm_sale))
        .route("/web/sales/{id}/payments", post(web_record_payment))
        .route("/web/sales/{id}/cancel", post(web_cancel_sale))
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
    use rust_decimal::Decimal;

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
            .foreign_keys(true)
            // Same posture as db::create_pool: customer triggers fire under REPLACE.
            .pragma("recursive_triggers", "1");
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

    async fn set_locale(state: &AppState, locale_code: &str, language_code: &str) {
        sqlx::query("INSERT OR IGNORE INTO business_locales (locale_code, language_code, display_name, is_enabled) VALUES (?, ?, ?, 1)")
            .bind(locale_code)
            .bind(language_code)
            .bind(locale_code)
            .execute(&state.pool)
            .await
            .unwrap();
        sqlx::query("INSERT OR IGNORE INTO business_settings (id, business_name, default_locale_code, currency_code, timezone) VALUES (1, 'Test', ?, 'USD', 'UTC')")
            .bind(locale_code)
            .execute(&state.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE business_settings SET default_locale_code = ? WHERE id = 1")
            .bind(locale_code)
            .execute(&state.pool)
            .await
            .unwrap();
    }

    async fn seed_customer(state: &AppState, name: &str) -> crate::models::Customer {
        state
            .customer_service
            .create_customer(
                audit_actor(state).await,
                crate::models::NewCustomer {
                    name: name.into(),
                    phone: None,
                    address: None,
                    tax_id: None,
                    notes: None,
                    is_walkin: false,
                    credit_limit: None,
                    due_days: None,
                },
            )
            .await
            .unwrap()
            .customer
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

    async fn post_form(app: axum::Router, uri: &str, body: &str) -> (StatusCode, String) {
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
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn web_sales_page_renders() {
        let app = crate::routes::router(test_state().await);
        let (status, html) = get_html(app, "/sales").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("Sales") || html.contains("sales"),
            "page should mention sales"
        );
        assert!(
            html.contains("Debt") || html.contains("debt"),
            "page should have debt section"
        );
    }

    #[tokio::test]
    async fn web_sale_list_fragment_renders() {
        let app = crate::routes::router(test_state().await);
        let (status, _) = get_html(app, "/web/sales").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn web_sale_debt_fragment_renders() {
        let app = crate::routes::router(test_state().await);
        let (status, html) = get_html(app, "/web/sales/debt").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("debt") || html.contains("Debt") || html.contains("OK"),
            "debt fragment should render: {html:.200}"
        );
    }

    // -- N2: the sale record page ---------------------------------------------

    /// Everything a record-page test needs to address the seeded document.
    struct RecordFixture {
        sale_id: i64,
        line_id: i64,
        product_id: i64,
        product_name: String,
        product_sku: String,
        account_id: i64,
        method_id: i64,
        account_name: String,
        method_name: String,
    }

    /// One draft sale with one line, plus an account configured with Cash, so a
    /// record-page test can drive draft, confirmed, paid and cancelled states.
    async fn seed_record_fixture(state: &AppState, payment_type: PaymentType) -> RecordFixture {
        use crate::models::{NewProduct, NewSale, ProductKind};
        use chrono::NaiveDate;
        use rust_decimal::Decimal;

        // A per-process suffix: one test may seed several fixtures, and the
        // product SKU must not collide across them (the purchase twin does
        // the same).
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let sku = format!("REC-SALE-{seq}");

        let product = state
            .inventory_service
            .create_product(
                audit_actor(&state).await,
                NewProduct {
                    sku,
                    name: "Record product".into(),
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
                },
            )
            .await
            .unwrap();
        let customer = seed_customer(state, "Record Buyer").await;
        let sale = state
            .sales_service
            .create_draft(
                audit_actor(&state).await,
                NewSale {
                    customer_id: customer.id,
                    payment_type,
                    sale_date: NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                    due_date: match payment_type {
                        PaymentType::Credit => Some(NaiveDate::from_ymd_opt(2024, 6, 2).unwrap()),
                        PaymentType::Cash => None,
                    },
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        let line = state
            .sales_service
            .add_line(
                audit_actor(&state).await,
                sale.id,
                product.id,
                Decimal::from(2),
                None,
            )
            .await
            .unwrap();
        // accounts.name is UNIQUE: suffix it per fixture, and still pass the
        // canonical "Caja" to the defaults helper so the Cash method seeds
        // (the purchase fixture does the same).
        let account = state
            .account_service
            .create(audit_actor(&state).await, &format!("Caja {seq}"))
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
            .find(|m| m.name == "Cash" && m.account_id == account.id)
            .expect("Cash is seeded by migrations");

        RecordFixture {
            sale_id: sale.id,
            line_id: line.id,
            product_id: product.id,
            product_name: product.name,
            product_sku: product.sku,
            account_id: account.id,
            method_id: method.id,
            account_name: account.name,
            method_name: method.name,
        }
    }

    #[tokio::test]
    async fn seeded_payment_method_labels_are_bilingual_and_ids_stay_canonical() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());

        set_locale(&state, "en-US", "en").await;
        let (status, html) = get_html(app.clone(), &format!("/sales/{}", fixture.sale_id)).await;
        assert_eq!(status, StatusCode::OK);
        let method_options = html.split("name=\"method_id\"").nth(1).unwrap_or(&html);
        assert!(method_options.contains("Cash — Caja"), "{method_options}");
        // There is no "— unassigned" option any more: migration 45 deleted the
        // history-less leftovers and made ownership NOT NULL, so every option
        // names an owner. What this test still guards is that the label is
        // translated AND that the id in the option stays the canonical row id.
        assert!(
            !method_options.contains("unassigned"),
            "no option may advertise an owner it does not have: {method_options}"
        );
        assert!(
            html.contains(&format!("value=\"{}\"", fixture.method_id)),
            "the canonical method id must remain unchanged: {html:.1200}"
        );

        set_locale(&state, "es-ES", "es").await;
        let (status, html) = get_html(app, &format!("/sales/{}", fixture.sale_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Efectivo — Caja"), "{html:.1200}");
        assert!(
            !html.contains("sin asignar"),
            "the Spanish catalog must not offer an ownerless method either: {html:.1200}"
        );
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

    /// Regression: the typed-id forms are gone. The sales list renders no
    /// `sale_id` input, drops the side-panel detail target and links every row
    /// to its record page, so an id is never typed. (redesign-interface N2)
    #[tokio::test]
    async fn web_sales_page_has_no_typed_id_forms_and_links_records() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state);

        let (status, html) = get_html(app, "/sales").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !html.contains("name=\"sale_id\""),
            "the sales list must not ask for a typed sale id: {html:.600}"
        );
        assert!(
            !html.contains("id=\"sale-detail\""),
            "the side-panel detail must be gone: {html:.600}"
        );
        assert!(
            html.contains(&format!("href=\"/sales/{}\"", fixture.sale_id)),
            "every row must link to its record: {html:.600}"
        );
    }

    /// AC4: creating a sale answers `HX-Redirect` to its record, so htmx
    /// performs a real navigation and no id is typed.
    #[tokio::test]
    async fn web_create_sale_redirects_to_the_record() {
        let state = test_state().await;
        let customer = seed_customer(&state, "Redirect Client").await;
        let app = crate::routes::router(state.clone());
        let body = format!(
            "customer_id={}&payment_type=Cash&sale_date=2024-05-02",
            customer.id
        );
        let req = Request::builder()
            .method("POST")
            .uri("/web/sales")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let redirect = resp
            .headers()
            .get("HX-Redirect")
            .expect("AC4: the create response must carry HX-Redirect")
            .to_str()
            .unwrap()
            .to_string();
        assert!(redirect.starts_with("/sales/"), "{redirect}");
        let sale_id: i64 = redirect["/sales/".len()..]
            .parse()
            .unwrap_or_else(|_| panic!("HX-Redirect must end in the sale id: {redirect}"));
        let detail = state.sales_service.get_detail(sale_id).await.unwrap();
        assert_eq!(detail.sale.customer_id, customer.id);
    }

    /// AC5 + name resolution: `/sales/{id}` is a real page inside the shell
    /// carrying names, not ids, for its line and payments; an unknown id is 404
    /// with the existing error shape.
    #[tokio::test]
    async fn web_sale_record_page_resolves_names_and_unknown_id_is_404() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        state
            .sales_service
            .confirm(audit_actor(&state).await, fixture.sale_id, None)
            .await
            .unwrap();
        state
            .sales_service
            .record_payment(
                audit_actor(&state).await,
                fixture.sale_id,
                fixture.method_id,
                Decimal::from(10),
                chrono::NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
            )
            .await
            .unwrap();
        let payment_id = state
            .sales_service
            .get_detail(fixture.sale_id)
            .await
            .unwrap()
            .payments[0]
            .id;
        let app = crate::routes::router(state);

        let (status, html) = get_html(app.clone(), &format!("/sales/{}", fixture.sale_id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("data-page-header"),
            "record uses the page header"
        );
        assert!(
            html.contains("data-nav=\"sales\"") && html.contains("aria-current=\"page\""),
            "record page keeps the sales nav key"
        );

        let line_row = row_with_id(&html, &format!("sale-line-{}", fixture.line_id));
        assert!(line_row.contains(&fixture.product_name), "{line_row}");
        assert!(line_row.contains(&fixture.product_sku), "{line_row}");
        assert!(!line_row.contains("product #"), "{line_row}");

        let payment_row = row_with_id(&html, &format!("sale-payment-{payment_id}"));
        assert!(payment_row.contains(&fixture.account_name), "{payment_row}");
        assert!(payment_row.contains(&fixture.method_name), "{payment_row}");
        assert!(!payment_row.contains("account #"), "{payment_row}");
        assert!(!payment_row.contains("method #"), "{payment_row}");

        let (status, body) = get_html(app, "/sales/999999").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("sale 999999 not found"), "{body}");
        assert!(!body.contains("route not found"), "{body}");
    }

    /// AC6: the actions offered match the document status. A draft can add a
    /// line, edit its header, confirm and discard; a confirmed credit sale can
    /// record payments and cancel but cannot edit lines or the header; a
    /// cancelled sale is read-only and shows its reason.
    #[tokio::test]
    async fn web_sale_record_actions_are_status_gated() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let app = crate::routes::router(state.clone());
        let base = format!("/web/sales/{}", fixture.sale_id);

        let (status, html) = get_html(app.clone(), &format!("/sales/{}", fixture.sale_id)).await;
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
            .sales_service
            .confirm(audit_actor(&state).await, fixture.sale_id, None)
            .await
            .unwrap();
        let (status, html) = get_html(app.clone(), &format!("/sales/{}", fixture.sale_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains(&format!("{base}/payments")),
            "a confirmed credit sale must offer payment recording"
        );
        assert!(html.contains(&format!("{base}/cancel")));
        assert!(
            !html.contains(&format!("{base}/lines")),
            "a confirmed sale must not edit lines"
        );
        assert!(
            !html.contains(&format!("{base}/header")),
            "a confirmed sale must not edit its header"
        );

        state
            .sales_service
            .cancel(
                audit_actor(&state).await,
                fixture.sale_id,
                Some("customer return".to_string()),
            )
            .await
            .unwrap();
        let (status, html) = get_html(app, &format!("/sales/{}", fixture.sale_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("customer return"),
            "a cancelled sale must show its reason: {html:.400}"
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
                "a cancelled sale must be read-only, found {target}"
            );
        }
    }

    /// Triangulation for AC6: a confirmed Cash sale is settled at confirm, so it
    /// offers cancel but no payment form, and its lines and header stay frozen.
    #[tokio::test]
    async fn web_sale_record_confirmed_cash_has_no_payment_form() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .sales_service
            .confirm(
                audit_actor(&state).await,
                fixture.sale_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state);
        let base = format!("/web/sales/{}", fixture.sale_id);

        let (status, html) = get_html(app, &format!("/sales/{}", fixture.sale_id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains(&format!("{base}/cancel")),
            "a confirmed cash sale can still be cancelled"
        );
        assert!(
            !html.contains(&format!("{base}/payments")),
            "a confirmed cash sale must not offer payment recording"
        );
        assert!(
            !html.contains(&format!("{base}/lines")),
            "a confirmed cash sale must not edit lines"
        );
        assert!(
            !html.contains(&format!("{base}/header")),
            "a confirmed cash sale must not edit its header"
        );
    }

    /// AC7: cancelling asks for confirmation before the request is sent; the
    /// confirm control carries `hx-confirm`. The same holds for discarding a
    /// draft.
    #[tokio::test]
    async fn web_sale_record_cancel_asks_for_confirmation() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let app = crate::routes::router(state.clone());

        let cancel_needle = format!("hx-post=\"/web/sales/{}/cancel\"", fixture.sale_id);
        let (status, html) = get_html(app.clone(), &format!("/sales/{}", fixture.sale_id)).await;
        assert_eq!(status, StatusCode::OK);
        let discard = element_tag_containing(&html, &cancel_needle);
        assert!(
            discard.contains("hx-confirm"),
            "discarding a draft must ask first: {discard}"
        );

        state
            .sales_service
            .confirm(audit_actor(&state).await, fixture.sale_id, None)
            .await
            .unwrap();
        let (status, html) = get_html(app, &format!("/sales/{}", fixture.sale_id)).await;
        assert_eq!(status, StatusCode::OK);
        let cancel = element_tag_containing(&html, &cancel_needle);
        assert!(
            cancel.contains("hx-confirm"),
            "cancelling a confirmed sale must ask first: {cancel}"
        );
    }

    /// The record-page actions swap the record body and keep the `sale-changed`
    /// refresh event, so the URL stays stable and subscribed regions update.
    #[tokio::test]
    async fn web_sale_record_header_edit_swaps_the_body_and_triggers_refresh() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());
        let body = "sale_date=2024-05-03&due_date=&receipt_no=ticket-9&notes=edited+note";
        let req = Request::builder()
            .method("POST")
            .uri(format!("/web/sales/{}/header", fixture.sale_id))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("HX-Trigger")
                .map(|v| v.to_str().unwrap()),
            Some("sale-changed")
        );
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let html = String::from_utf8_lossy(&bytes).to_string();
        assert!(html.contains("sale-record-inner"), "{html:.400}");
        assert!(html.contains("edited note"), "{html:.400}");
        let detail = state
            .sales_service
            .get_detail(fixture.sale_id)
            .await
            .unwrap();
        assert_eq!(detail.sale.notes, "edited note");
        assert_eq!(detail.sale.sale_date.to_string(), "2024-05-03");
    }

    /// Behavioral: the payment form posts to the collection endpoint with the
    /// sale id in the body; the payment must land on that sale, not another.
    #[tokio::test]
    async fn web_collection_payment_records_on_the_sale_in_the_body() {
        use crate::models::{
            MovementReason, MovementType, NewMovement, NewProduct, NewSale, PaymentType,
            ProductKind,
        };
        use rust_decimal::Decimal;

        let state = test_state().await;
        let app = crate::routes::router(state.clone());

        let product = state
            .inventory_service
            .create_product(
                audit_actor(&state).await,
                NewProduct {
                    sku: "WEB-PAY".into(),
                    name: "prod WEB-PAY".into(),
                    kind: ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: Decimal::from(10),
                    cost_price: Decimal::from(5),
                    track_stock: true,
                    min_stock: Some(Decimal::ZERO),
                    max_stock: Some(Decimal::from(100)),
                    location: None,
                    notes: None,
                    markup_pct: None,
                },
            )
            .await
            .unwrap();
        state
            .inventory_service
            .record_movement(
                audit_actor(&state).await,
                NewMovement {
                    product_id: product.id,
                    qty: Decimal::from(10),
                    movement_type: MovementType::In,
                    reason: MovementReason::Initial,
                    reference: String::new(),
                    date: chrono::NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
                },
            )
            .await
            .unwrap();
        let account = state
            .account_service
            .create(
                audit_actor(&state).await,
                &format!("Caja {}", test_support::fixture_seq()),
            )
            .await
            .unwrap();
        state
            .payment_method_service
            .ensure_defaults_for_account(audit_actor(&state).await, account.id, "Caja")
            .await
            .unwrap();
        let cash = state
            .payment_method_service
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|m| m.name == "Cash" && m.account_id == account.id)
            .expect("Cash method is seeded by migrations");

        let payer = seed_customer(&state, "Web Payer").await;

        let mut sale_ids = Vec::new();
        for _ in 0..2 {
            let sale = state
                .sales_service
                .create_draft(
                    audit_actor(&state).await,
                    NewSale {
                        customer_id: payer.id,
                        payment_type: PaymentType::Credit,
                        sale_date: chrono::NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                        due_date: Some(chrono::NaiveDate::from_ymd_opt(2024, 6, 2).unwrap()),
                        receipt_no: None,
                        notes: None,
                    },
                )
                .await
                .unwrap();
            state
                .sales_service
                .add_line(
                    audit_actor(&state).await,
                    sale.id,
                    product.id,
                    Decimal::from(2),
                    None,
                )
                .await
                .unwrap();
            state
                .sales_service
                .confirm(audit_actor(&state).await, sale.id, None)
                .await
                .unwrap();
            sale_ids.push(sale.id);
        }
        let (paid, untouched) = (sale_ids[0], sale_ids[1]);

        // Same body shape the rendered payment form submits.
        let body = format!(
            "sale_id={paid}&account_id={}&method_id={}&amount=10.00&date=2024-05-02",
            account.id, cash.id
        );
        let req = Request::builder()
            .method("POST")
            .uri("/web/sales/payments")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let paid_detail = state.sales_service.get_detail(paid).await.unwrap();
        assert_eq!(
            paid_detail.payments.len(),
            1,
            "payment must reach sale {paid}"
        );
        let untouched_detail = state.sales_service.get_detail(untouched).await.unwrap();
        assert!(
            untouched_detail.payments.is_empty(),
            "sale {untouched} must stay unpaid"
        );
    }

    /// Triangulation: lines/confirm/cancel collection endpoints also act on the
    /// sale id from the body and leave the other sale untouched.
    #[tokio::test]
    async fn web_collection_endpoints_act_on_the_body_sale_id() {
        use crate::models::{NewProduct, NewSale, PaymentType, ProductKind, SaleStatus};
        use rust_decimal::Decimal;

        let state = test_state().await;
        let app = crate::routes::router(state.clone());

        let product = state
            .inventory_service
            .create_product(
                audit_actor(&state).await,
                NewProduct {
                    sku: "WEB-SVC".into(),
                    name: "svc WEB-SVC".into(),
                    kind: ProductKind::Service,
                    category_id: None,
                    unit: "hr".into(),
                    sale_price: Decimal::from(30),
                    cost_price: Decimal::ZERO,
                    track_stock: false,
                    min_stock: None,
                    max_stock: None,
                    location: None,
                    notes: None,
                    markup_pct: None,
                },
            )
            .await
            .unwrap();

        let typist = seed_customer(&state, "Web Typist").await;

        let mut sale_ids = Vec::new();
        for _ in 0..2 {
            let sale = state
                .sales_service
                .create_draft(
                    audit_actor(&state).await,
                    NewSale {
                        customer_id: typist.id,
                        payment_type: PaymentType::Credit,
                        sale_date: chrono::NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                        due_date: Some(chrono::NaiveDate::from_ymd_opt(2024, 6, 2).unwrap()),
                        receipt_no: None,
                        notes: None,
                    },
                )
                .await
                .unwrap();
            sale_ids.push(sale.id);
        }
        let (target, other) = (sale_ids[0], sale_ids[1]);

        // Add line: only the body's sale gains a line.
        let (status, _) = post_form(
            app.clone(),
            "/web/sales/lines",
            &format!("sale_id={target}&product_id={}&qty=1", product.id),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            state
                .sales_service
                .get_detail(target)
                .await
                .unwrap()
                .lines
                .len(),
            1
        );
        assert_eq!(
            state
                .sales_service
                .get_detail(other)
                .await
                .unwrap()
                .lines
                .len(),
            0
        );

        // Confirm: only the body's sale leaves Draft.
        let (status, _) = post_form(
            app.clone(),
            "/web/sales/confirm",
            &format!("sale_id={target}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            state
                .sales_service
                .get_detail(target)
                .await
                .unwrap()
                .sale
                .status,
            SaleStatus::Confirmed
        );
        assert_eq!(
            state
                .sales_service
                .get_detail(other)
                .await
                .unwrap()
                .sale
                .status,
            SaleStatus::Draft
        );

        // Cancel: only the body's sale is cancelled.
        let (status, _) = post_form(
            app.clone(),
            "/web/sales/cancel",
            &format!("sale_id={target}&reason=changed+mind"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            state
                .sales_service
                .get_detail(target)
                .await
                .unwrap()
                .sale
                .status,
            SaleStatus::Cancelled
        );
        assert_eq!(
            state
                .sales_service
                .get_detail(other)
                .await
                .unwrap()
                .sale
                .status,
            SaleStatus::Draft
        );

        // Legacy path-based route still works with the id in the path and no
        // sale_id field in the body (shared form struct stays backward compatible).
        let (status, _) = post_form(
            app.clone(),
            &format!("/web/sales/{other}/lines"),
            &format!("product_id={}&qty=1", product.id),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            state
                .sales_service
                .get_detail(other)
                .await
                .unwrap()
                .lines
                .len(),
            1
        );
    }

    /// The non-HTMX form path also lands on the record page, so a browser
    /// without htmx (or a plain POST) still never sees a list to retype an id
    /// from.
    #[tokio::test]
    async fn web_create_sale_redirects_a_plain_form_to_the_record() {
        let state = test_state().await;
        let customer = seed_customer(&state, "Plain Client").await;
        let app = crate::routes::router(state);
        let body = format!(
            "customer_id={}&payment_type=Cash&sale_date=2024-05-02",
            customer.id
        );
        let req = Request::builder()
            .method("POST")
            .uri("/web/sales")
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
        assert!(location.starts_with("/sales/"), "{location}");
    }

    // -- K2: the sale form carries a mandatory customer -----------------------

    /// AC2: the selector defaults to the walk-in, so a cash sale needs no choice.
    #[tokio::test]
    async fn k2_sale_form_defaults_to_the_walkin_customer() {
        let app = crate::routes::router(test_state().await);
        let (status, html) = get_html(app, "/sales").await;
        assert_eq!(status, StatusCode::OK);
        let select_start = html
            .find("name=\"customer_id\"")
            .expect("the sale form must render a customer selector");
        let select_end = html[select_start..]
            .find("</select>")
            .expect("customer selector must close");
        let select = &html[select_start..select_start + select_end];
        let selected_option = select
            .split("<option")
            .find(|option| option.contains("selected"))
            .expect("the walk-in option must carry selected");
        assert!(
            selected_option.contains("Consumidor final"),
            "the preselected option must be the walk-in: {selected_option}"
        );
    }

    /// AC2: omitting the customer in the form is a 400, never an ownerless sale.
    #[tokio::test]
    async fn k2_sale_form_without_customer_is_rejected() {
        let app = crate::routes::router(test_state().await);
        let (status, body) =
            post_form(app, "/web/sales", "payment_type=Cash&sale_date=2024-05-02").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.to_lowercase().contains("customer"), "{body}");
    }

    // -- N4: the product picker on the record page ----------------------------

    /// Cuts the `<form>...</form>` region that contains `needle`, for structural
    /// assertions such as "the results container is not inside the picker form".
    fn enclosing_form<'a>(html: &'a str, needle: &str) -> &'a str {
        let pos = html
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} not rendered: {html:.600}"));
        let start = html[..pos]
            .rfind("<form")
            .expect("needle must sit in a form");
        let end = html[pos..].find("</form>").expect("form must close");
        &html[start..pos + end + "</form>".len()]
    }

    /// The catalogue `<select>` is replaced by the picker island: the page
    /// renders the `[data-picker]` mount point with its calling context, one
    /// add-line form with the server's contract, and a sibling results
    /// container. The island owns the search, so the page carries no
    /// declarative transport — no verb, trigger, target, vals or keyup handler
    /// — and the debounce lives in `static/picker.js`, not in markup. Escape
    /// is base.html's document-level keydown handler, shared by both pickers.
    #[tokio::test]
    async fn n4_sale_record_offers_the_picker_instead_of_the_catalogue_select() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state);

        let (status, html) = get_html(app, &format!("/sales/{}", fixture.sale_id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            !html.contains("<select name=\"product_id\""),
            "the whole-catalogue select must be gone: {html:.800}"
        );

        let picker = enclosing_form(&html, "id=\"product-picker\"");
        assert!(
            picker.contains(&format!("hx-post=\"/web/sales/{}/lines\"", fixture.sale_id)),
            "{picker}"
        );
        assert!(
            picker.contains("name=\"product_id\""),
            "the form carries the island-owned hidden product id: {picker}"
        );
        assert!(
            picker.contains("name=\"qty\"") && picker.contains("value=\"1\""),
            "a scan and a click must both carry the default quantity: {picker}"
        );

        // The field carries no declarative search transport: the island owns
        // the read, the debounce and the Escape semantics.
        let input_pos = html
            .find("id=\"product-picker\"")
            .expect("the record page renders the picker field");
        let input_start = html[..input_pos]
            .rfind('<')
            .expect("the id must sit inside a tag");
        let input_end = input_pos + html[input_pos..].find('>').expect("unterminated tag");
        let input_tag = &html[input_start..=input_end];
        for transport in [
            "hx-get",
            "hx-trigger",
            "hx-target",
            "hx-vals",
            "hx-on:keyup",
        ] {
            assert!(
                !input_tag.contains(transport),
                "the field must not carry {transport}: {input_tag}"
            );
        }

        // The island's mount point carries its calling context: one picker,
        // priced for a sale.
        let container_pos = html
            .find("data-picker")
            .expect("the record page renders the island mount point");
        let container_start = html[..container_pos]
            .rfind('<')
            .expect("the attribute must sit inside a tag");
        let container_end =
            container_start + html[container_start..].find('>').expect("unterminated tag");
        let container_tag = &html[container_start..=container_end];
        assert!(
            container_tag.contains("id=\"line-picker\"")
                && container_tag.contains("data-price-kind=\"sale\""),
            "{container_tag}"
        );

        // The debounce moved with the island: the island file declares it, so
        // the search cannot lose its debounce by a markup edit alone.
        assert!(
            include_str!("../../static/picker.js").contains("DEBOUNCE_MS = 250"),
            "the search must be debounced by static/picker.js"
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
            html.contains("id=\"sale-record-money\""),
            "adding a line swaps the money region, which carries the total and the lines"
        );
    }

    /// AC9 + AC10: an exact barcode submits the line in one step, and the same
    /// response carries the updated lines, the running total and an out-of-band
    /// picker that is empty and focused.
    #[tokio::test]
    async fn n4_sale_line_scan_adds_in_one_step_and_resets_the_picker() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .inventory_service
            .add_barcode(fixture.product_id, "7791234567890")
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        // Exactly what the picker form posts on Enter: the typed value and the
        // quantity, no product id and no click.
        let (status, added) = post_form(
            app,
            &format!("/web/sales/{}/lines", fixture.sale_id),
            "product=7791234567890&qty=2&unit_price=",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{added}");

        let detail = state
            .sales_service
            .get_detail(fixture.sale_id)
            .await
            .unwrap();
        assert_eq!(detail.lines.len(), 2, "the scan adds its own line");
        let scanned = detail
            .lines
            .iter()
            .find(|line| line.qty == Decimal::from(2))
            .expect("the scanned line");
        assert_eq!(scanned.product_id, fixture.product_id);

        // One response carries the lines, the running total and the OOB picker, so
        // lines and total can never drift.
        assert!(added.contains(&fixture.product_name), "{added:.600}");
        assert!(
            added.contains("50 USD"),
            "the running total travels with the lines: {added:.800}"
        );
        // Locate the picker by ITS OWN id, never by the first `hx-swap-oob` in
        // the document. This response now carries more than one OOB element: the
        // payment status chip rides out of band too, and it sits higher in the
        // header than the picker does. "The first OOB element" therefore finds
        // whichever happens to come first in page order — a property of the
        // markup rather than of the behaviour under test. The same lesson is
        // already written down in `assert_oob_picker_is_empty_and_focused`.
        let picker_pos = added
            .find("id=\"line-picker\"")
            .unwrap_or_else(|| panic!("the picker must come back: {added:.600}"));
        let tag_start = added[..picker_pos].rfind('<').unwrap();
        let oob = &added[tag_start..];
        assert!(
            oob.contains("autofocus"),
            "the picker must come back focused: {oob:.400}"
        );
        let input_pos = oob.find("id=\"product-picker\"").unwrap();
        let input_start = oob[..input_pos].rfind('<').unwrap();
        let input_end = input_pos + oob[input_pos..].find('>').unwrap();
        let input_tag = &oob[input_start..=input_end];
        assert!(
            !input_tag.contains("value="),
            "the picker must come back empty: {input_tag}"
        );
    }

    /// AC10 (clicked result): a result is its own add action; the request includes
    /// the picker form, so the quantity travels, and supplies the product id
    /// itself. The typed text is not an exact match on purpose.
    #[tokio::test]
    async fn n4_sale_line_clicked_result_uses_the_picker_quantity() {
        use rust_decimal::Decimal;

        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());

        let (status, body) = post_form(
            app,
            &format!("/web/sales/{}/lines", fixture.sale_id),
            &format!(
                "product=record&qty=3&unit_price=&product_id={}",
                fixture.product_id
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let detail = state
            .sales_service
            .get_detail(fixture.sale_id)
            .await
            .unwrap();
        let clicked = detail
            .lines
            .iter()
            .find(|line| line.qty == Decimal::from(3))
            .expect("the clicked line");
        assert_eq!(clicked.product_id, fixture.product_id);
        assert_eq!(detail.total, Decimal::from(125));
    }

    /// AC12: an unresolvable value is a clear 400 that names the number of partial
    /// matches the search found, and it adds nothing.
    #[tokio::test]
    async fn n4_sale_line_unknown_value_is_400_and_adds_nothing() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());
        let before = state
            .sales_service
            .get_detail(fixture.sale_id)
            .await
            .unwrap();

        let (status, body) = post_form(
            app,
            &format!("/web/sales/{}/lines", fixture.sale_id),
            "product=record&qty=1&unit_price=",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("no exact match"), "{body}");
        assert!(
            body.contains("1 match"),
            "the message must name the search count: {body}"
        );

        let after = state
            .sales_service
            .get_detail(fixture.sale_id)
            .await
            .unwrap();
        assert_eq!(
            after.lines.len(),
            before.lines.len(),
            "a failed resolution adds no line"
        );
        assert_eq!(after.total, before.total);
    }

    // -- S6 enforcement (AC10): the permission gates on the real handlers ------

    /// Like [`post_form`], but with an explicit cookie and optional headers:
    /// an empty `HX-Request` set means the plain browser post the full-page
    /// refusal shape needs.
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

    /// A principal holding ONLY `sales.read` opens the reads and is refused
    /// every web mutation, each in the shape its caller reads and naming its
    /// own code: the draft lifecycle `sales.create`, cancelling
    /// `sales.cancel`, and money received `customers.collect`.
    #[tokio::test]
    async fn ac10_a_sales_read_only_principal_is_refused_the_web_mutations_in_both_shapes() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["sales.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        // The reads the probe is allowed: the page, the record and the fragments.
        for uri in [
            "/sales".to_string(),
            format!("/sales/{}", fixture.sale_id),
            "/web/sales".to_string(),
            "/web/sales/debt".to_string(),
            format!("/web/sales/{}", fixture.sale_id),
        ] {
            let (status, html) = get_html_as(app.clone(), &uri, Some(&cookie)).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {html:.200}");
        }

        // Creating a sale over HTMX: JSON naming the recording gate.
        let (status, body) = post_form_as(
            app.clone(),
            "/web/sales",
            &format!("customer_id={}&payment_type=Cash", fixture.sale_id),
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(
            body.contains("sales.create"),
            "the HTMX refusal must name sales.create: {body}"
        );

        // The same create as a plain browser post: the HTML refusal card.
        let (status, body) = post_form_as(
            app.clone(),
            "/web/sales",
            "customer_id=1&payment_type=Cash",
            &[],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body:.400}");
        assert!(
            body.contains("Action not permitted"),
            "the refusal must use the English fallback: {body:.400}"
        );
        assert!(
            body.contains("sales.create"),
            "the refusal must name the missing permission: {body:.400}"
        );

        // The record actions: lines and header are the recording gate...
        for (uri, body) in [
            (
                format!("/web/sales/{}/lines", fixture.sale_id),
                format!("product_id={}&qty=1", fixture.product_id),
            ),
            (
                format!("/web/sales/{}/header", fixture.sale_id),
                "notes=hacked".to_string(),
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
                body.contains("sales.create"),
                "{uri} must name sales.create: {body}"
            );
        }

        // ...confirm is the recording tier...
        let (status, body) = post_form_as(
            app.clone(),
            &format!("/web/sales/{}/confirm", fixture.sale_id),
            "method_id=",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(body.contains("sales.create"), "{body}");

        // Line edit and removal are the recording gate too, each in its own
        // method (the web routes register POST for the line edit, DELETE for
        // the removal) so a swapped extractor on either endpoint fails exactly
        // one assertion.
        for (method, uri, body) in [
            (
                "POST",
                format!("/web/sales/{}/lines/{}", fixture.sale_id, fixture.line_id),
                "qty=9&unit_price=9",
            ),
            (
                "DELETE",
                format!("/web/sales/{}/lines/{}", fixture.sale_id, fixture.line_id),
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
                text.contains("sales.create"),
                "{method} {uri} must name sales.create: {text}"
            );
        }

        // ...the payment is the collection tier...
        let (status, body) = post_form_as(
            app.clone(),
            &format!("/web/sales/{}/payments", fixture.sale_id),
            &format!("method_id={}&amount=5&date=2024-05-03", fixture.method_id),
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(
            body.contains("customers.collect"),
            "the refusal must name customers.collect: {body}"
        );

        // ...and the cancel is its own tier.
        let (status, body) = post_form_as(
            app,
            &format!("/web/sales/{}/cancel", fixture.sale_id),
            "reason=",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(body.contains("sales.cancel"), "{body}");
    }

    /// The refusal writes nothing: the refused confirmation keeps the draft,
    /// the refused payment writes no payment row, and a refused creation
    /// leaves the sales table where it was.
    #[tokio::test]
    async fn ac10_the_sales_web_refusal_writes_nothing() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let probe = test_support::seed_session_with_permissions(
            &state.pool,
            &["sales.read", "customers.read"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        let sales_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sales")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let (status, body) = post_form_as(
            app.clone(),
            "/web/sales",
            "customer_id=1&payment_type=Cash",
            &[],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body:.200}");
        let sales_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sales")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(
            sales_after, sales_before,
            "a refused create must write nothing"
        );

        let (status, body) = post_form_as(
            app.clone(),
            &format!("/web/sales/{}/confirm", fixture.sale_id),
            "method_id=",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let after = state
            .sales_service
            .get_detail(fixture.sale_id)
            .await
            .unwrap();
        assert_eq!(
            after.sale.status,
            crate::models::SaleStatus::Draft,
            "a refused confirm must not flip the status"
        );

        state
            .sales_service
            .confirm(audit_actor(&state).await, fixture.sale_id, None)
            .await
            .unwrap();
        let payments_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sale_payments")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let (status, body) = post_form_as(
            app,
            &format!("/web/sales/{}/payments", fixture.sale_id),
            &format!("method_id={}&amount=5&date=2024-05-03", fixture.method_id),
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let payments_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sale_payments")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(
            payments_after, payments_before,
            "a refused payment must write nothing"
        );
    }

    /// A principal holding the permissions gets the normal answers: the
    /// creation redirects to the new record and the record actions answer
    /// their fragments.
    #[tokio::test]
    async fn ac10_the_sales_web_holding_principal_gets_the_normal_answer() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let holder = test_support::seed_session_with_permissions(
            &state.pool,
            &[
                "sales.read",
                "sales.create",
                "sales.cancel",
                "customers.read",
                "customers.collect",
            ],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&holder);
        let app = crate::routes::router(state.clone());

        let (status, _body) = post_form_as(
            app.clone(),
            "/web/sales",
            &format!(
                "customer_id={}&payment_type=Cash&sale_date=2024-05-02",
                fixture.sale_id
            ),
            &[],
            Some(&cookie),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "the creation redirects to the record"
        );

        let (status, body) = post_form_as(
            app.clone(),
            &format!("/web/sales/{}/confirm", fixture.sale_id),
            "method_id=",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.300}");

        let (status, body) = post_form_as(
            app.clone(),
            &format!("/web/sales/{}/payments", fixture.sale_id),
            &format!("method_id={}&amount=10&date=2024-05-03", fixture.method_id),
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.300}");

        let (status, _body) = post_form_as(
            app,
            &format!("/web/sales/{}/cancel", fixture.sale_id),
            "reason=customer return",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    /// The old collection endpoints (id in the body) are separate gated
    /// boundaries: the adapter declares its own gate and the delegated call is
    /// a plain function call, so removing the ADAPTER's gate is exactly what
    /// this test pins (the path endpoints pin the inner handlers above).
    #[tokio::test]
    async fn the_sales_collection_adapters_carry_their_own_gate() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let probe = test_support::seed_session_with_permissions(
            &state.pool,
            &["sales.read", "customers.read"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state);

        let cases = [
            (
                "/web/sales/lines",
                format!(
                    "sale_id={}&product_id={}&qty=1",
                    fixture.sale_id, fixture.product_id
                ),
                "sales.create",
            ),
            (
                "/web/sales/confirm",
                format!("sale_id={}&method_id=", fixture.sale_id),
                "sales.create",
            ),
            (
                "/web/sales/payments",
                format!(
                    "sale_id={}&method_id={}&amount=5&date=2024-05-03",
                    fixture.sale_id, fixture.method_id
                ),
                "customers.collect",
            ),
            (
                "/web/sales/cancel",
                format!("sale_id={}&reason=", fixture.sale_id),
                "sales.cancel",
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

    /// The read gates are real too: a principal WITHOUT `sales.read` (it holds
    /// an unrelated permission, so this is not a broken fixture) is refused
    /// every sales page and fragment with the full-page refusal card.
    #[tokio::test]
    async fn the_read_gates_refuse_a_principal_without_the_read_permission() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Credit).await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["customers.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state);

        for uri in [
            "/sales",
            &format!("/sales/{}", fixture.sale_id),
            "/web/sales",
            "/web/sales/debt",
            &format!("/web/sales/{}", fixture.sale_id),
        ] {
            let (status, html) = get_html_as(app.clone(), uri, Some(&cookie)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {html:.200}");
            assert!(
                html.contains("Action not permitted") && html.contains("sales.read"),
                "{uri} must refuse naming sales.read: {html:.300}"
            );
        }
    }

    /// The gate order must not change: an anonymous request gets the login
    /// redirect, never the permission refusal.
    #[tokio::test]
    async fn an_anonymous_request_still_gets_the_login_gate_not_the_permission_refusal() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let (status, _) = post_form_as(app.clone(), "/web/sales", "customer_id=1", &[], None).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        // get_html always carries the shared cookie, so the anonymous GET is
        // built by hand here.
        let req = Request::builder()
            .method("GET")
            .uri("/sales")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    }

    // -- DELETE /web/sales/{id}: the documents drawer's draft delete -----------

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
    async fn web_delete_draft_sale_answers_200_with_the_sale_changed_trigger() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());

        let (status, body, trigger) = send_delete(
            app,
            &format!("/web/sales/{}", fixture.sale_id),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            trigger.as_deref(),
            Some("sale-changed"),
            "the documents page listens for sale-changed"
        );
        let err = state
            .sales_service
            .get_detail(fixture.sale_id)
            .await
            .unwrap_err();
        assert!(
            matches!(err, crate::error::AppError::NotFound(_)),
            "the deleted draft must be gone: {err:?}"
        );
    }

    #[tokio::test]
    async fn web_delete_draft_refuses_a_confirmed_sale_with_400() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .sales_service
            .confirm(
                audit_actor(&state).await,
                fixture.sale_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, body, _) = send_delete(
            app,
            &format!("/web/sales/{}", fixture.sale_id),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(state
            .sales_service
            .get_detail(fixture.sale_id)
            .await
            .is_ok());
    }

    /// A discarded sale (cancelled before confirm, number still NULL)
    /// deletes through the same route: empty 200, `sale-changed` trigger,
    /// detail then 404s — the acceptance path for never-confirmed cancelled
    /// rows.
    #[tokio::test]
    async fn web_delete_draft_discarded_cancelled_sale_answers_200_and_is_gone() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .sales_service
            .cancel(audit_actor(&state).await, fixture.sale_id, None)
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, body, trigger) = send_delete(
            app,
            &format!("/web/sales/{}", fixture.sale_id),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            trigger.as_deref(),
            Some("sale-changed"),
            "the documents page listens for sale-changed"
        );
        let err = state
            .sales_service
            .get_detail(fixture.sale_id)
            .await
            .unwrap_err();
        assert!(
            matches!(err, crate::error::AppError::NotFound(_)),
            "the deleted discarded sale must be gone: {err:?}"
        );
    }

    /// Confirmed-then-cancelled keeps the protection at the route: 400 and
    /// the row survives.
    #[tokio::test]
    async fn web_delete_draft_refuses_a_confirmed_then_cancelled_sale_with_400() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .sales_service
            .confirm(
                audit_actor(&state).await,
                fixture.sale_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        state
            .sales_service
            .cancel(
                audit_actor(&state).await,
                fixture.sale_id,
                Some("wrong order".to_string()),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, body, _) = send_delete(
            app,
            &format!("/web/sales/{}", fixture.sale_id),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(state
            .sales_service
            .get_detail(fixture.sale_id)
            .await
            .is_ok());
    }

    /// An unknown id is 404 through the same route — the service's NotFound
    /// shape, unchanged by the predicate widening.
    #[tokio::test]
    async fn web_delete_draft_unknown_sale_is_404() {
        let state = test_state().await;
        let app = crate::routes::router(state);

        let (status, body, _) =
            send_delete(app, "/web/sales/999999", Some(test_support::TEST_COOKIE)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    }

    /// T3: the record page offers Delete — with the native confirm — ONLY for
    /// a discarded (never-confirmed, number still NULL) cancelled sale.
    /// A confirmed-then-cancelled record carries its number and must show no
    /// delete at all.
    #[tokio::test]
    async fn web_sale_record_offers_delete_only_for_a_discarded_cancelled_sale() {
        let state = test_state().await;
        let discarded = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .sales_service
            .cancel(audit_actor(&state).await, discarded.sale_id, None)
            .await
            .unwrap();

        let annulled = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .sales_service
            .confirm(
                audit_actor(&state).await,
                annulled.sale_id,
                Some(annulled.method_id),
            )
            .await
            .unwrap();
        state
            .sales_service
            .cancel(
                audit_actor(&state).await,
                annulled.sale_id,
                Some("wrong order".to_string()),
            )
            .await
            .unwrap();

        let app = crate::routes::router(state);

        let (status, html) = get_html(app.clone(), &format!("/sales/{}", discarded.sale_id)).await;
        assert_eq!(status, StatusCode::OK);
        let needle = format!("hx-delete=\"/web/sales/{}\"", discarded.sale_id);
        assert!(
            html.contains(&needle),
            "a discarded sale must offer delete: {html:.400}"
        );
        assert!(
            element_tag_containing(&html, &needle).contains("hx-confirm"),
            "deleting must ask first"
        );

        let (status, html) = get_html(app, &format!("/sales/{}", annulled.sale_id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !html.contains(&format!("hx-delete=\"/web/sales/{}\"", annulled.sale_id)),
            "a confirmed-then-cancelled record must offer no delete: {html:.400}"
        );
    }

    #[tokio::test]
    async fn web_delete_draft_requires_sales_create() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());
        let probe = test_support::seed_session_with_permissions(&state.pool, &["sales.read"])
            .await
            .unwrap();

        let (status, _, _) = send_delete(
            app,
            &format!("/web/sales/{}", fixture.sale_id),
            Some(&test_support::cookie_for(&probe)),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(state
            .sales_service
            .get_detail(fixture.sale_id)
            .await
            .is_ok());
    }

    // -----------------------------------------------------------------------
    // Tax-contract overflow (tax contract overflow T1): a draft line whose
    // arithmetic leaves the representable range must be REFUSED, in the
    // operator's language, on a real request.
    //
    // There is no `catch_unwind` and no panic-catching layer in this crate, so
    // a panic inside the handler resolves the per-connection task to a dropped
    // `JoinHandle`: the HTMX post paints nothing and the operator's typed
    // values look like they vanished. Every test in this block therefore
    // asserts a STATUS and a SENTENCE, because "did not panic" is not
    // observable from the outside.
    // -----------------------------------------------------------------------

    /// A product of its own, so a line test can address the line it wrote: the
    /// fixture already holds a line for ITS product, and two lines on one product
    /// make "the line I added" ambiguous.
    async fn product_with_sku(state: &AppState, sku: &str) -> i64 {
        let product = state
            .inventory_service
            .create_product(
                audit_actor(state).await,
                crate::models::NewProduct {
                    sku: sku.into(),
                    name: format!("Overflow {sku}"),
                    kind: crate::models::ProductKind::Product,
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
                },
            )
            .await
            .unwrap();
        product.id
    }

    /// A product with one linked active tax, at the given rate, for a line test
    /// that needs the tax half of the contract to run.
    async fn link_one_tax(state: &AppState, product_id: i64, code: &str, rate: &str) {
        let actor = audit_actor(state).await;
        let tax = state
            .tax_service
            .create_tax(
                actor,
                crate::models::NewTax {
                    code: code.into(),
                    name: format!("Tax {code}"),
                    rate: Decimal::from_str(rate).unwrap(),
                    is_active: true,
                },
            )
            .await
            .unwrap();
        state
            .tax_service
            .link_product_tax(actor, product_id, tax.id)
            .await
            .unwrap();
    }

    /// The line amount is `qty * unit_price` and BOTH operands are unbounded, so
    /// the raw product the repository evaluates BEFORE the tax contract can
    /// leave the 96-bit range on its own: `1e20 * 1e9` is `1e29`. That is a
    /// separate panic from the contract's, and it is the one an operator types
    /// into a quantity box.
    #[tokio::test]
    async fn a_sale_line_whose_amount_overflows_is_a_localized_refusal_not_a_panic() {
        let state = test_state().await;
        set_locale(&state, "es-AR", "es").await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let app = crate::routes::router(state.clone());
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        let (status, body) = post_form(
            app,
            &format!("/web/sales/{}/lines", fixture.sale_id),
            &format!(
                "product_id={}&qty=100000000000000000000&unit_price=1000000000",
                fixture.product_id
            ),
        )
        .await;

        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "an unrepresentable line amount is a refusal, not a dropped connection: {body}"
        );
        let expected =
            localization.tr(crate::localization::MessageKey::PriceRefusalLineAmountTooLarge);
        assert!(
            body.contains(&expected),
            "the operator must read the refusal in their own language: {body}"
        );
        assert!(
            !body.contains("\"error\":\"validation error\""),
            "the refusal reaches the operator as the sentence, not as a wrapped error: {body}"
        );
        // The refused line wrote nothing: the fixture's own line is the only one.
        assert_eq!(
            state
                .sales_service
                .get_detail(fixture.sale_id)
                .await
                .unwrap()
                .lines
                .len(),
            1
        );
    }

    /// PROBE C, and the case a multiply-only fix would still crash on.
    ///
    /// `qty = 1` and `unit_price = Decimal::MAX` make the line amount itself
    /// representable, and a 1% rate makes `net * rate` representable too — the
    /// contribution is `MAX / 100`, which fits. The running tax total fits. Only
    /// the final `net + tax_total` leaves the range, because `MAX + MAX/100` is
    /// `1.5 * MAX`. Every multiply passes, so a fix that checks the multiplies
    /// and forgets the add panics here on the very next line.
    #[tokio::test]
    async fn a_sale_line_whose_tax_total_cannot_be_added_to_its_net_is_a_refusal() {
        let state = test_state().await;
        set_locale(&state, "es-AR", "es").await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        link_one_tax(&state, fixture.product_id, "ONEPC", "1").await;
        let app = crate::routes::router(state.clone());
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();

        let (status, body) = post_form(
            app,
            &format!("/web/sales/{}/lines", fixture.sale_id),
            &format!(
                "product_id={}&qty=1&unit_price=79228162514264337593543950335",
                fixture.product_id
            ),
        )
        .await;

        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "every multiply fits here: only the final add leaves the range, and it must still be \
             a refusal: {body}"
        );
        let expected =
            localization.tr(crate::localization::MessageKey::PriceRefusalTaxArithmeticTooLarge);
        assert!(
            body.contains(&expected),
            "the two refusals are different rules and the operator must be told which one fired: \
             {body}"
        );
        assert_eq!(
            state
                .sales_service
                .get_detail(fixture.sale_id)
                .await
                .unwrap()
                .lines
                .len(),
            1,
            "a refused line writes nothing"
        );
    }

    /// THE CONTROL. The bound is a BOUND, not a blanket rejection: an amount
    /// this large is legal, so the line must be written and its tax computed
    /// exactly like any other. Without this test a fix that refused everything
    /// above some arbitrary ceiling would be green.
    #[tokio::test]
    async fn a_large_but_representable_sale_line_is_still_computed_exactly() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        let product_id = product_with_sku(&state, "SALE-BIG-OK").await;
        link_one_tax(&state, product_id, "TENC", "10").await;
        let app = crate::routes::router(state.clone());

        // qty 1e6 * unit_price 1e13 = a net of 1e19: twenty digits, far above
        // anything a business means by a price, comfortably inside the 29 the
        // `Decimal` range carries, and a 10% tax whose contribution (1e18) and
        // final add (1.1e19) both fit with room to spare.
        let (status, body) = post_form(
            app,
            &format!("/web/sales/{}/lines", fixture.sale_id),
            &format!("product_id={product_id}&qty=1000000&unit_price=10000000000000"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let detail = state
            .sales_service
            .get_detail(fixture.sale_id)
            .await
            .unwrap();
        let line = detail
            .lines
            .iter()
            .find(|line| line.product_id == product_id)
            .expect("the line must be written");
        let dec = |raw: &str| rust_decimal::Decimal::from_str(raw).unwrap();
        assert_eq!(line.subtotal(), dec("10000000000000000000"));
        assert_eq!(
            line.tax_total,
            dec("1000000000000000000"),
            "a representable amount is priced by the same contract as any other, exactly"
        );
    }

    // -----------------------------------------------------------------------
    // Document-level accumulation (tax contract overflow T3).
    //
    // Every test above is PER LINE. This block is the DOCUMENT: a sum of lines
    // that each pass the per-line write bound and still cannot be added up.
    // Per-line carryability — the invariant T1 and T2 established — says nothing
    // about a sum, so no per-line rule can refuse this construction: both lines
    // are stored by the real checked write.
    //
    // The tests are the operator's, not the arithmetic's: they drive the real
    // router with the real session cookie and assert a STATUS and a SENTENCE.
    // "Did not panic" is not observable from outside the process — a panic
    // escapes the handler, the connection is dropped and the operator is left
    // with nothing at all — so a test that only proved "no panic" would be
    // satisfied by a page that renders an empty document.
    // -----------------------------------------------------------------------

    /// `4e28`: an amount an operator can type into a price box, individually
    /// carryable (`Decimal::MAX ≈ 7.92e28`), accepted by the checked line write
    /// and computable by the tax contract. Two of them are the SMALLEST
    /// construction that cannot be added up: `4e28 + 4e28 = 8e28 > MAX`.
    const FOUR_E28: &str = "40000000000000000000000000000";

    /// A draft sale with NO lines, plus a product of its own, so the document is
    /// exactly the lines the test adds. `seed_record_fixture` seeds a line of its
    /// own, which would be a third term in a total this test is about.
    async fn empty_draft_sale(state: &AppState, sku: &str) -> (i64, i64) {
        use crate::models::{NewProduct, NewSale, ProductKind};
        use chrono::NaiveDate;

        let product = state
            .inventory_service
            .create_product(
                audit_actor(state).await,
                NewProduct {
                    sku: sku.into(),
                    name: format!("Document total {sku}"),
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
                },
            )
            .await
            .unwrap();
        let customer = seed_customer(state, "Document Total Buyer").await;
        let sale = state
            .sales_service
            .create_draft(
                audit_actor(state).await,
                NewSale {
                    customer_id: customer.id,
                    payment_type: PaymentType::Cash,
                    sale_date: NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        (sale.id, product.id)
    }

    /// THE CONSTRUCTION. Two lines, each individually carryable, whose SUM is
    /// not. The second line's INSERT COMMITS before the response renders, so
    /// from that moment the document exists and every surface that reads it must
    /// answer: an unreadable document is stranded data, not a rejected request.
    #[tokio::test]
    async fn a_draft_sale_whose_lines_cannot_be_added_up_is_still_readable() {
        let state = test_state().await;
        set_locale(&state, "es-AR", "es").await;
        let (sale_id, product_id) = empty_draft_sale(&state, "DOC-TOTAL-2LINE").await;
        let app = crate::routes::router(state.clone());
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        // The refusal in the operator's own language, through the ONE shared
        // mapping every price refusal uses — the same sentence the add-line
        // response, the record page and the index must all show.
        let expected = localization
            .tr(crate::localization::MessageKey::PriceRefusalDocumentTotalTooLarge)
            .to_string();
        assert!(
            !expected.is_empty()
                && expected != crate::models::PriceRefusal::DocumentTotalTooLarge.as_str(),
            "the Spanish catalog carries a sentence of its own for this rule"
        );

        // One line of 4e28: the document totals exactly, so the record renders
        // and this response is a plain 200 with no refusal anywhere on it.
        let (status, body) = post_form(
            app.clone(),
            &format!("/web/sales/{sale_id}/lines"),
            &format!("product_id={product_id}&qty=1&unit_price={FOUR_E28}"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "one line of 4e28 is carryable on its own, and the per-line write bound is a BOUND \
             rather than a blanket rejection: {body}"
        );
        assert!(
            !body.contains("data-document-total-refusal"),
            "a document that totals says nothing about the rule: {body:.800}"
        );

        // The second line is the one that commits before the fold runs.
        let (status, body) = post_form(
            app.clone(),
            &format!("/web/sales/{sale_id}/lines"),
            &format!("product_id={product_id}&qty=1&unit_price={FOUR_E28}"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the add-line response itself is one of the four surfaces, and a document whose total \
             cannot be carried is still a document: {body}"
        );
        assert!(
            body.contains(&expected),
            "the add-line response states the refusal in the operator's language: {body:.2000}"
        );
        assert_eq!(
            body.matches("id=\"sale-line-").count(),
            2,
            "and it still shows BOTH lines, because each of them is representable: {body:.2000}"
        );

        // And the record page, which is the operator's way back into it.
        let (status, html) = get_html(app.clone(), &format!("/sales/{sale_id}")).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the document is READABLE: an operator who cannot open a stored document cannot fix \
             it: {html:.600}"
        );
        assert!(
            html.contains(&expected),
            "the record page states the refusal: {html:.2000}"
        );
        assert_eq!(
            html.matches("id=\"sale-line-").count(),
            2,
            "the operator can see WHICH lines the document carries: {html:.2000}"
        );
        assert!(
            html.contains("data-sale-payment-status=\"refused\""),
            "and that no payment status is published for a total that does not exist: {html:.2000}"
        );

        // AND THE DOCUMENT IS NOT STRANDED: it is still a working draft. Removing
        // one line brings the sum back inside the range, and the page says so by
        // rendering the total again. This is the whole difference between a
        // document that is unreadable and one that is too large.
        let line_id: i64 =
            sqlx::query_scalar("SELECT id FROM sale_lines WHERE sale_id = ? ORDER BY id LIMIT 1")
                .bind(sale_id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        let (status, body, _) = send_delete(
            app.clone(),
            &format!("/web/sales/{sale_id}/lines/{line_id}"),
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.600}");
        assert!(
            !body.contains(&expected),
            "one line less and the document totals again: the refusal was about the document, not \
             about the line that was removed: {body:.2000}"
        );

        // The stored document is the ordinary one the tests around it expect.
        let detail = state.sales_service.get_detail(sale_id).await.unwrap();
        assert_eq!(detail.lines.len(), 1);
        assert_eq!(detail.total, Decimal::from_str(FOUR_E28).unwrap());
    }

    /// THE CASE A NET-ONLY GUARD MISSES.
    ///
    /// With an ordinary 21% tax the per-multiply bound caps a line's net at
    /// `MAX / 21 ≈ 3.77e27`, so the two-line construction is out of reach: the
    /// write refuses a line that big. Eighteen lines of `3.7e27` get there
    /// instead, and they bind on a DIFFERENT accumulation than the two-line
    /// case does — the net sums to `6.66e28` and the tax to `1.3986e28`, both
    /// inside the range, and only the sum of the lines' tax-inclusive totals
    /// (`18 × 4.477e27 = 8.0586e28`) leaves it.
    ///
    /// The claim is asserted here in CHECKED form rather than assumed, so the
    /// test says which accumulation overflows and stops claiming the case exists
    /// if `Decimal`'s limits ever move.
    #[tokio::test]
    async fn a_draft_sale_whose_tax_inclusive_total_cannot_be_added_up_is_still_readable() {
        let state = test_state().await;
        set_locale(&state, "es-AR", "es").await;
        let (sale_id, product_id) = empty_draft_sale(&state, "DOC-TOTAL-18LINE").await;
        link_one_tax(&state, product_id, "IVA21D", "21").await;
        let app = crate::routes::router(state.clone());

        // The eighteen lines' own arithmetic, in checked form: every line is
        // written, and only the DOCUMENT's tax-inclusive total is out of range.
        let net = Decimal::from_str("3700000000000000000000000000").unwrap();
        let tax = Decimal::from_str("777000000000000000000000000").unwrap();
        let line_total = Decimal::from_str("4477000000000000000000000000").unwrap();
        assert_eq!(tax, net * Decimal::from(21) / Decimal::from(100));
        assert_eq!(line_total, net + tax);
        assert!(
            net.checked_mul(Decimal::from(18)).is_some(),
            "the NET must fit, or this is not the case it claims to be"
        );
        assert!(
            tax.checked_mul(Decimal::from(18)).is_some(),
            "the TAX must fit, or this is not the case it claims to be"
        );
        assert!(
            line_total.checked_mul(Decimal::from(18)).is_none(),
            "the tax-inclusive line totals must be the accumulation that leaves the range"
        );

        for line in 0..18 {
            let (status, body) = post_form(
                app.clone(),
                &format!("/web/sales/{sale_id}/lines"),
                &format!("product_id={product_id}&qty=1&unit_price={}", dec_str(net)),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::OK,
                "line {} of 18 is individually carryable: the write bound is per LINE, and a \
                 document that reaches 18 lines was accepted 17 times over: {body}",
                line + 1
            );
        }

        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let expected = localization
            .tr(crate::localization::MessageKey::PriceRefusalDocumentTotalTooLarge)
            .to_string();
        let (status, html) = get_html(app, &format!("/sales/{sale_id}")).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the document is READABLE: {html:.600}"
        );
        assert!(
            html.contains(&expected),
            "the tax-inclusive total is the accumulation that leaves the range, and the operator \
             reads the same refusal for it: {html:.2000}"
        );
        assert_eq!(
            html.matches("id=\"sale-line-").count(),
            18,
            "every line is still shown, and each line's own taxed total is intact: {html:.2000}"
        );
        // The service agrees about WHICH accumulation refused, and says so
        // through its own typed result rather than through the page.
        match state.sales_service.get_detail(sale_id).await {
            Err(crate::error::AppError::PriceRefused(refusal)) => assert_eq!(
                refusal,
                crate::models::PriceRefusal::DocumentTotalTooLarge,
                "the tax-inclusive accumulation is the one that refuses, and it is its own rule"
            ),
            other => panic!("the document total cannot be computed: {other:?}"),
        }
    }

    /// THE ACTION PATHS. A document whose total cannot be computed cannot be
    /// confirmed, paid or discarded: each of those measures money against the
    /// tax-inclusive total, and each must refuse BEFORE it writes. The assertions
    /// that matter are the ones after the request — a refusal that leaves a
    /// numbered document, a stock movement or a finance row behind is a refusal
    /// the operator cannot trust, and an annulment that refunds half of a
    /// document and then refuses is worse than a refusal.
    ///
    /// The refusal is RECOVERABLE, and this test proves the two ways out rather
    /// than asserting the word "recoverable": the record page is readable (the
    /// test above), so a line can be removed from it, and a never-confirmed
    /// draft can be deleted outright from the documents drawer.
    #[tokio::test]
    async fn a_sale_whose_total_cannot_be_computed_refuses_every_action_with_no_side_effect() {
        let state = test_state().await;
        set_locale(&state, "es-AR", "es").await;
        let (sale_id, product_id) = empty_draft_sale(&state, "DOC-TOTAL-ACTIONS").await;
        let app = crate::routes::router(state.clone());
        for _ in 0..2 {
            post_form(
                app.clone(),
                &format!("/web/sales/{sale_id}/lines"),
                &format!("product_id={product_id}&qty=1&unit_price={FOUR_E28}"),
            )
            .await;
        }
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let expected = localization
            .tr(crate::localization::MessageKey::PriceRefusalDocumentTotalTooLarge)
            .to_string();

        let (status, body) = post_form(
            app.clone(),
            &format!("/web/sales/{sale_id}/confirm"),
            "method_id=",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "confirm measures the cash scenario against the document total: {body}"
        );
        assert!(
            body.contains(&expected),
            "in the operator's language: {body}"
        );

        let sale: (String, Option<String>) =
            sqlx::query_as("SELECT status, sale_number FROM sales WHERE id = ?")
                .bind(sale_id)
                .fetch_one(&state.pool)
                .await
                .unwrap();
        assert_eq!(
            sale,
            ("Draft".to_string(), None),
            "a refused confirm writes NOTHING: no number, no status change"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM transactions")
                .fetch_one(&state.pool)
                .await
                .unwrap(),
            0,
            "and no finance row"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM stock_movements")
                .fetch_one(&state.pool)
                .await
                .unwrap(),
            0,
            "and no stock movement"
        );

        // The discard refuses the same way, and writes nothing either.
        let (status, body) = post_form(
            app.clone(),
            &format!("/web/sales/{sale_id}/cancel"),
            "reason=x",
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a discard measures the same total: {body}"
        );
        assert!(body.contains(&expected), "in their language: {body}");
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT status FROM sales WHERE id = ?")
                .bind(sale_id)
                .fetch_one(&state.pool)
                .await
                .unwrap(),
            "Draft",
            "and a refused discard changes no status: a half-applied annulment is worse than a \
             refusal"
        );

        // The way out that needs no total at all: a never-confirmed draft is
        // deletable, and the delete never reads the document's money.
        state.sales_service.delete_draft(sale_id).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sales WHERE id = ?")
                .bind(sale_id)
                .fetch_one(&state.pool)
                .await
                .unwrap(),
            0,
            "so the operator is never stuck holding a document they cannot act on"
        );
    }

    /// THE CONTROL, and the reason a bound that refuses everything large is not
    /// a fix. Twenty ordinary taxed lines are added up exactly as before: the
    /// checked path is a different IMPLEMENTATION of the same sum, never a
    /// different rule about when a document may be totalled.
    #[tokio::test]
    async fn many_ordinary_lines_still_total_exactly() {
        let state = test_state().await;
        let (sale_id, product_id) = empty_draft_sale(&state, "DOC-TOTAL-CONTROL").await;
        link_one_tax(&state, product_id, "IVA21C", "21").await;
        let app = crate::routes::router(state.clone());

        for _ in 0..20 {
            let (status, body) = post_form(
                app.clone(),
                &format!("/web/sales/{sale_id}/lines"),
                &format!("product_id={product_id}&qty=3&unit_price=12.34"),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }

        // 20 x (3 x 12.34 = 37.02 net, 21% = 7.77 tax, 44.79 tax-inclusive).
        let (status, html) = get_html(app, &format!("/sales/{sale_id}")).await;
        assert_eq!(status, StatusCode::OK, "{html:.600}");
        let detail = state.sales_service.get_detail(sale_id).await.unwrap();
        assert_eq!(detail.net_subtotal, Decimal::from_str("740.40").unwrap());
        assert_eq!(detail.tax_total, Decimal::from_str("155.40").unwrap());
        assert_eq!(detail.total, Decimal::from_str("895.80").unwrap());
        assert_eq!(detail.lines.len(), 20);
    }

    /// A `Decimal` as the form field carries it, for a test that computes the
    /// price it types instead of repeating the digits.
    fn dec_str(value: Decimal) -> String {
        value.normalize().to_string()
    }

    // -----------------------------------------------------------------------
    // List surfaces (tax contract overflow T3, F1).
    //
    // A list is the one surface where a refusal must NOT be the answer: the page
    // holds many documents, and one document that cannot be totaled must render
    // IN PLACE — with its identity, with no figure, and with the refusal where
    // the figure was — while every other row renders exactly as it does today.
    // Refusing the page would hide 49 documents behind 1.
    //
    // The document under test here is an ordinary DRAFT: the real add-line form
    // stores it, so nothing here needs a hand-written row. The debt banner's
    // test below needs a CONFIRMED one, and that one does smuggle a line in
    // through SQL — because a confirmation refuses an un-totalable document by
    // design, so no application path can produce it.
    // -----------------------------------------------------------------------

    /// One ordinary line, added to a draft through the real form. Returns the
    /// draft's id.
    ///
    /// The price is a whole number on purpose: these tests run under `es-AR`,
    /// whose form parser takes `,` as the decimal separator, so a `12.34` field
    /// is a form error there and the fixture would fail before reaching the
    /// behaviour under test.
    async fn ordinary_draft(state: &AppState, sku: &str) -> i64 {
        let (sale_id, product_id) = empty_draft_sale(state, sku).await;
        let (status, body) = post_form(
            crate::routes::router(state.clone()),
            &format!("/web/sales/{sale_id}/lines"),
            &format!(
                "product_id={product_id}&qty=2&unit_price={}",
                ORDINARY_PRICE
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        sale_id
    }

    /// The ordinary line's money: `qty 2` at `1234`, and the amount the page is
    /// expected to render for it in whatever locale the test set.
    const ORDINARY_PRICE: &str = "1234";

    /// One row of a rendered list, sliced out by its row id — so an assertion
    /// about "the refused row" is about THAT row and not about the page.
    ///
    /// The slice ends at the row's own "Open" link, which is the last element of
    /// a row's trailing zone. Cutting there keeps a comparison between two renders
    /// about the ROW and not about whatever follows it on the page, which is
    /// exactly what differs when one render carries an extra document.
    fn row_html<'a>(html: &'a str, id_prefix: &str, id: i64) -> &'a str {
        let marker = format!("id=\"{id_prefix}{id}\"");
        let start = html
            .find(&marker)
            .unwrap_or_else(|| panic!("row {id_prefix}{id} is missing from the page"))
            + marker.len();
        let rest = &html[start..];
        let end = rest.find("</a>").map(|at| at + 4).unwrap_or(0);
        &rest[..end]
    }

    /// A draft sale carrying two lines of `4e28`, stored by the real form.
    async fn untotalable_draft(state: &AppState, sku: &str) -> i64 {
        let (sale_id, product_id) = empty_draft_sale(state, sku).await;
        for _ in 0..2 {
            let (status, body) = post_form(
                crate::routes::router(state.clone()),
                &format!("/web/sales/{sale_id}/lines"),
                &format!("product_id={product_id}&qty=1&unit_price={FOUR_E28}"),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        sale_id
    }

    /// THE LIST PAGE ANSWERS. One un-totalable draft among ordinary ones: the
    /// page renders, the bad document renders in place with the refusal and
    /// without a figure, and the ordinary documents are untouched.
    #[tokio::test]
    async fn the_sales_list_renders_a_document_whose_total_cannot_be_computed() {
        let state = test_state().await;
        set_locale(&state, "es-AR", "es").await;
        let first = ordinary_draft(&state, "LIST-ORD-1").await;
        let second = ordinary_draft(&state, "LIST-ORD-2").await;
        let third = ordinary_draft(&state, "LIST-ORD-3").await;
        let untotalable = untotalable_draft(&state, "LIST-BAD").await;
        let app = crate::routes::router(state.clone());
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let expected = localization
            .tr(crate::localization::MessageKey::PriceRefusalDocumentTotalTooLarge)
            .to_string();
        let big = localization.format_currency(Decimal::from_str(FOUR_E28).unwrap());

        let (status, html) = get_html(app, "/sales").await;
        assert_eq!(
            status,
            StatusCode::OK,
            "one document that cannot be totaled must not take the page down: {html:.800}"
        );

        // THE REFUSED ROW: in place, identified, and carrying the sentence.
        let row = row_html(&html, "sale-", untotalable);
        assert!(
            row.contains(&expected),
            "the refused row states the rule in the operator's language: {row:.1200}"
        );
        assert!(
            // The list row's identity is the PARTY, not the product: a document
            // row is what the operator scans, and the products are one click away
            // on the document itself.
            row.contains("Document Total Buyer"),
            "and keeps its identity: {row:.1200}"
        );
        assert!(
            row.contains(localization.tr(crate::localization::MessageKey::StatusDraft)),
            "including its status, in the operator's language: {row:.1200}"
        );
        assert!(
            !row.contains(&big),
            "and shows NO amount: a stale, partial or zero figure is worse than none, because the \
             operator cannot tell a real zero from a refusal. row: {row:.1200}"
        );
        assert!(
            !row.contains("0.00"),
            "and in particular no zero placeholder: {row:.1200}"
        );

        // THE ORDINARY ROWS: every one is still there, with its own money.
        let ordinary_amount = localization.format_currency(Decimal::from_str("2468").unwrap());
        for id in [first, second, third] {
            let row = row_html(&html, "sale-", id);
            assert!(
                row.contains(&ordinary_amount),
                "an ordinary document still shows its own total: {row:.1200}"
            );
        }
    }

    /// THE CONTROL, and the only honest form of "exactly as today": the SAME
    /// ordinary documents, rendered by the SAME code, with and without the
    /// un-totalable one on the page. Their rows must be byte-identical, so the
    /// refusal path cannot have changed a single character of a normal row.
    #[tokio::test]
    async fn an_ordinary_sales_list_is_unchanged_by_the_presence_of_a_refused_row() {
        let with_bad = test_state().await;
        set_locale(&with_bad, "es-AR", "es").await;
        let a1 = ordinary_draft(&with_bad, "CTRL-ORD-1").await;
        let a2 = ordinary_draft(&with_bad, "CTRL-ORD-2").await;
        untotalable_draft(&with_bad, "CTRL-BAD").await;

        let without_bad = test_state().await;
        set_locale(&without_bad, "es-AR", "es").await;
        let b1 = ordinary_draft(&without_bad, "CTRL-ORD-1").await;
        let b2 = ordinary_draft(&without_bad, "CTRL-ORD-2").await;
        assert_eq!((a1, a2), (b1, b2), "the control seeds the same rows");

        let (status, dirty) = get_html(crate::routes::router(with_bad), "/sales").await;
        assert_eq!(status, StatusCode::OK, "{dirty:.400}");
        let (status, clean) = get_html(crate::routes::router(without_bad), "/sales").await;
        assert_eq!(status, StatusCode::OK, "{clean:.400}");

        for id in [a1, a2] {
            assert_eq!(
                row_html(&dirty, "sale-", id),
                row_html(&clean, "sale-", id),
                "an ordinary row is byte-identical with and without a refused document on the page"
            );
        }
        assert!(
            !clean.contains("data-document-total-refusal"),
            "and an all-ordinary page states nothing about the rule: {clean:.2000}"
        );
    }

    /// A CONFIRMED credit sale whose lines cannot be added up, for the surfaces
    /// that only read confirmed documents. A confirmation refuses an un-totalable
    /// document by design, so the second line is inserted the one way nothing in
    /// this application is supposed to: straight through SQL, past the checked
    /// write. That is precisely the case the checked accumulation exists for.
    async fn confirmed_untotalable_credit_sale(state: &AppState, sku: &str) -> (i64, i64) {
        use crate::models::{NewSale, PaymentType as Pay};
        use chrono::NaiveDate;
        use rust_decimal::Decimal as Dec;

        let actor = audit_actor(state).await;
        let product = state
            .inventory_service
            .create_product(
                actor,
                crate::models::NewProduct {
                    sku: sku.into(),
                    name: format!("Debt {sku}"),
                    kind: crate::models::ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: Dec::from(25),
                    cost_price: Dec::from(10),
                    track_stock: false,
                    min_stock: None,
                    max_stock: None,
                    location: None,
                    notes: None,
                    markup_pct: None,
                },
            )
            .await
            .unwrap();
        let customer = seed_customer(state, "Debt Buyer").await;
        let sale = state
            .sales_service
            .create_draft(
                actor,
                NewSale {
                    customer_id: customer.id,
                    payment_type: Pay::Credit,
                    sale_date: NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 2).unwrap()),
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        // One line of 4e28 totals exactly, so the real flow confirms it.
        state
            .sales_service
            .add_line(
                audit_actor(&state).await,
                sale.id,
                product.id,
                Dec::from(1),
                Some(Dec::from_str(FOUR_E28).unwrap()),
            )
            .await
            .unwrap();
        state
            .sales_service
            .confirm(actor, sale.id, None)
            .await
            .expect("one line of 4e28 is carryable, so the confirm is an ordinary one");
        // The second line: stored directly, which is the only way a CONFIRMED
        // document can carry two lines the document fold cannot add up.
        sqlx::query(
            "INSERT INTO sale_lines (sale_id, product_id, qty, unit_price, tax_total) \
             VALUES (?, ?, ?, ?, 0)",
        )
        .bind(sale.id)
        .bind(product.id)
        .bind(Dec::from(1).to_string())
        .bind(FOUR_E28)
        .execute(&state.pool)
        .await
        .unwrap();
        (sale.id, customer.id)
    }

    /// THE DEBT BANNER, which is a DIFFERENT aggregation from a plain list: its
    /// headline is a sum over every unpaid document in the shop, and its rows
    /// are documents. The page must render both — the sentence where the total
    /// was, the document rows in place — because a receivable that silently
    /// omitted one invoice would misstate what the shop is owed.
    #[tokio::test]
    async fn the_debt_banner_renders_a_document_whose_total_cannot_be_computed() {
        let state = test_state().await;
        set_locale(&state, "es-AR", "es").await;
        let (untotalable, _) = confirmed_untotalable_credit_sale(&state, "DEBT-BAD").await;
        let ordinary = ordinary_draft(&state, "DEBT-ORD").await;
        let app = crate::routes::router(state.clone());
        let localization = crate::localization::load_context(&state.pool)
            .await
            .unwrap();
        let expected = localization
            .tr(crate::localization::MessageKey::PriceRefusalDocumentTotalTooLarge)
            .to_string();
        let big = localization.format_currency(Decimal::from_str(FOUR_E28).unwrap());

        let (status, html) = get_html(app, "/web/sales/debt").await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the banner is a sum over documents, and one un-totalable document must not empty the \
             panel: {html:.800}"
        );
        assert!(
            html.contains(&expected),
            "the headline states the refusal instead of a partial total: {html:.1200}"
        );
        assert!(
            !html.contains(&big),
            "and publishes no figure for the set: {html:.1200}"
        );
        // The document itself is still listed in the banner, in place.
        let row = row_html(&html, "sale-debt-", untotalable);
        assert!(
            row.contains(&expected),
            "the document's own row states the rule too: {row:.1200}"
        );
        assert!(!row.contains(&big), "and no figure: {row:.1200}");
        let _ = ordinary;
    }

    // -- Credit note: the action that starts a customer return -----------------

    /// **THE TEST THAT WOULD HAVE CAUGHT THE DEAD CREATION FLOW.** A credit note
    /// reverses a NAMED document, so its creation post carries that document's
    /// id, and nothing else in the app can supply one. The record page of the sale
    /// being credited is the only place the operator is already holding it, so the
    /// action lives there with the id rendered onto it, filled in.
    #[tokio::test]
    async fn a_confirmed_sale_record_offers_issue_credit_note_with_this_sale_id_already_filled() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .sales_service
            .confirm(
                audit_actor(&state).await,
                fixture.sale_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let app = crate::routes::router(state);

        let (status, html) = get_html(app, &format!("/sales/{}", fixture.sale_id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");

        let form = enclosing_form(&html, "/web/customer-returns");
        assert!(
            form.contains(&format!("value=\"{}\"", fixture.sale_id)),
            "the action must carry THIS sale's id, filled in: a credit note names the \
             document it credits and nothing else can supply that id: {form:.600}"
        );
        // The LABEL, not just the mechanics. "Credit note" is the document's name
        // and it replaced "Take goods back" on 2026-10-01, when decision 5 of the
        // design was reversed: the old label named a warehouse action, and what
        // this button starts is a document. Reversible by changing this one string
        // and this one assertion — nothing else in the tree holds the label, and
        // `the_customer_returns_index_renders_the_localized_title_in_both_catalogs`
        // pins the page half of the same decision.
        assert!(
            form.contains("Credit note"),
            "the action names the DOCUMENT it starts: {form:.600}"
        );
    }

    /// The same two-sided gate the purchase twin has: the action is
    /// `sales.create`, the parent's own write code. A principal that may read a
    /// sale and may not create one sees no action and is refused the post.
    #[tokio::test]
    async fn issue_credit_note_is_gated_on_sales_create_for_both_sight_and_post() {
        let state = test_state().await;
        let fixture = seed_record_fixture(&state, PaymentType::Cash).await;
        state
            .sales_service
            .confirm(
                audit_actor(&state).await,
                fixture.sale_id,
                Some(fixture.method_id),
            )
            .await
            .unwrap();
        let probe = test_support::seed_session_with_permissions(&state.pool, &["sales.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state);

        let (status, html) = get_html_as(
            app.clone(),
            &format!("/sales/{}", fixture.sale_id),
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            !html.contains("/web/customer-returns"),
            "a principal without sales.create must not be shown an action the route \
             refuses: {html:.600}"
        );

        let (status, html) = post_form_as(
            app,
            "/web/customer-returns",
            &format!("sale_id={}", fixture.sale_id),
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{html:.300}");
        assert!(
            html.contains("sales.create"),
            "the refusal names the code the principal lacks: {html:.600}"
        );
    }

    /// The audit-attribution element's own text, and nothing else.
    ///
    /// Scoped deliberately: the page CHROME also renders the signed-in user's
    /// display name, so a whole-page `contains("Test Admin")` is true before any
    /// edit happens and would let this test pass for the wrong reason. The
    /// assertion has to be about the attribution line alone.
    fn sale_audit_line(html: &str) -> String {
        let start = html
            .find("data-sale-actor>")
            .unwrap_or_else(|| panic!("the record page must render the attribution line"))
            + "data-sale-actor>".len();
        let end = html[start..]
            .find("</div>")
            .expect("the attribution element is a single div")
            + start;
        html[start..end].to_string()
    }

    /// A LINE EDIT IS ATTRIBUTED END TO END.
    ///
    /// The record page's audit line is a statement about a DOCUMENT, and the
    /// action that turns an operator into that document's editor is the one
    /// that has to change it. Before the fix the sale family never stamped the
    /// parent on a line write, so this line kept reading `Registered by …`
    /// alone after the operator had just edited the document — and the header
    /// WAS refreshed (the out-of-band swap on `sale_detail.html:134` exists for
    /// exactly this) and still named nobody.
    ///
    /// Two real users, because a stamp that always equalled the creator would
    /// satisfy a single-user test: `Zoe Creadora` opens the draft, and the
    /// request is made by the seeded session principal, `Test Admin`. The
    /// assertion is on the NAME the page renders, because the name is what an
    /// operator reads and what the wiring layer resolves.
    #[tokio::test]
    async fn a_line_edit_names_the_operator_who_made_it_in_the_rendered_audit_line() {
        use crate::models::{NewProduct, NewSale, ProductKind};
        use chrono::NaiveDate;
        let state = test_state().await;
        let creator = test_support::seed_audit_user(&state.pool, "sale-web-zoe", "Zoe Creadora")
            .await
            .unwrap();
        let product = state
            .inventory_service
            .create_product(
                audit_actor(&state).await,
                NewProduct {
                    sku: "WEB-AUDIT".into(),
                    name: "Web Audit Product".into(),
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
                },
            )
            .await
            .unwrap();
        let customer = seed_customer(&state, "Web Audit Buyer").await;
        let sale = state
            .sales_service
            .create_draft(
                creator,
                NewSale {
                    customer_id: customer.id,
                    payment_type: PaymentType::Cash,
                    sale_date: NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                    due_date: None,
                    receipt_no: None,
                    notes: None,
                },
            )
            .await
            .unwrap();
        let line = state
            .sales_service
            .add_line(creator, sale.id, product.id, Decimal::from(2), None)
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        // BEFORE: the only writer so far is Zoe, so the line names her and NOT
        // the principal who is about to edit the document.
        let (status, html) = get_html(app.clone(), &format!("/sales/{}", sale.id)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        let before_line = sale_audit_line(&html);
        assert!(
            before_line.contains("Zoe Creadora"),
            "the record page names the draft's creator: {before_line:.300}"
        );
        assert!(
            !before_line.contains("Test Admin"),
            "and names no editor yet — nobody but Zoe has touched it: {before_line:.300}"
        );

        // THE EDIT, through the real route as the seeded principal.
        let (status, body) = post_form_as(
            app.clone(),
            &format!("/web/sales/{}/lines/{}", sale.id, line.id),
            "qty=3&unit_price=30",
            &[("HX-Request", "true")],
            Some(test_support::TEST_COOKIE),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the line edit: {body:.400}");

        // The rendered audit line names the EDITOR. This is the assertion the
        // defect made impossible: the same fragment, the same document, and a
        // name that is neither the creator's nor absent.
        let after_line = sale_audit_line(&body);
        assert!(
            after_line.contains("Test Admin"),
            "the audit line names the operator who edited the line: {after_line:.300}"
        );
        assert!(
            after_line.contains("Zoe Creadora"),
            "and still names the creator: a line edit is not a re-registration: {after_line:.300}"
        );
        assert_ne!(
            after_line, before_line,
            "the same document's attribution line CHANGED: the action that made \
             the operator its editor is the action that has to say so"
        );

        // The STORED value agrees with the rendered name: the route resolved the
        // actor from its own `Principal`, not from the fixture.
        let stored = state.sales_service.get_detail(sale.id).await.unwrap().sale;
        assert_eq!(stored.created_by, creator, "the creator is never rewritten");
        assert_ne!(
            stored.updated_by,
            Some(creator),
            "the editor is not the creator: this test would pass for the wrong \
             reason if it were"
        );
        // The principal's id is deliberately NOT read back here: AC20 forbids a
        // department file from querying an identity table, and a test is still a
        // department file — AC20 greps the whole source, comments included, so
        // even naming the query here would fail the guard. The attribution is
        // already pinned without it — the RENDERED line names "Test Admin",
        // which is the seeded session's display name and which no fixture in
        // this test created — and what the stored value adds is that the stamp
        // is neither the creator nor the fixture's own audit actor.
        let fixture_actor = audit_actor(&state).await;
        assert_ne!(
            fixture_actor, creator,
            "the fixture's audit actor and the draft's creator must differ, \
             otherwise the two exclusions below prove nothing"
        );
        assert_ne!(
            stored.updated_by,
            Some(fixture_actor),
            "the document does not name the fixture's sentinel actor: the \
             stamp came from the REQUEST's principal"
        );
        assert!(
            stored.updated_by.is_some(),
            "a successful line write leaves a document with an editor"
        );

        // And the REMOVAL, through the same route, is attributed the same way:
        // a removal is an edit of the document too. A SECOND principal makes
        // that leg independently load-bearing — re-using `Test Admin` would
        // assert nothing, because the editor already IS that name from the
        // update above, so a removal which stamped nothing would still render
        // it. `Test Probe` can only appear here if the removal itself carried
        // the request's actor through.
        let editor_after_update = stored.updated_by;
        let probe_token =
            test_support::seed_session_with_permissions(&state.pool, &["sales.create"])
                .await
                .unwrap();
        let probe_cookie = test_support::cookie_for(&probe_token);
        let (status, body, _) = send_delete(
            app.clone(),
            &format!("/web/sales/{}/lines/{}", sale.id, line.id),
            Some(&probe_cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the line removal: {body:.400}");
        let after_removal = sale_audit_line(&body);
        assert!(
            after_removal.contains("Test Probe"),
            "removing a line is an edit, so the audit line names THAT operator: \
             the rendered line is {after_removal:.300}"
        );
        assert!(
            !after_removal.contains("Test Admin"),
            "and the previous editor is no longer named — otherwise this leg \
             proves nothing: the rendered line is {after_removal:.300}"
        );
        let editor_after_removal = state
            .sales_service
            .get_detail(sale.id)
            .await
            .unwrap()
            .sale
            .updated_by;
        assert_ne!(
            editor_after_removal, editor_after_update,
            "the STORED editor changed with the removal: this assertion is on \
             the value, not on how long anything took"
        );
        assert!(
            editor_after_removal.is_some(),
            "and a successful removal still leaves a document with an editor"
        );
    }
}
