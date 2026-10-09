// M4 customers (Slice M). Web customers under `/customers` and
// `/customers/{id}`, Askama + HTMX, thin handlers over `CustomerService`
// (entity + rules), `SalesService` (derived receivable) and
// `CustomerReceiptService` (collect). The list page shows names only; the
// per-customer detail (statement + collect form + receipts) lives in
// `partials/customer_detail.html` and renders both inside the slide-over
// drawer and as the id-final `/web/customers/detail/{id}` fragment the
// drawer loads (id-final so the row links stay data-bound record links under
// the wiring guard). Creation lives in a `<dialog>` modal. Fragments live in
// `partials/customer_list.html`, `partials/customer_detail.html`,
// `partials/customer_statement.html` and `partials/receipt_list.html`.
// Creation lives in a `<dialog>` modal; each list row also carries an ✎
// button loading the prefilled `partials/customer_edit_form.html` fragment
// (`/web/customers/edit-form/{id}`, id-final like the detail fragment so the
// id-free list page keeps no concrete non-final segment under the wiring
// guard) into a second dialog, posting to the existing `/web/customers/edit`
// backend.
//
// Typed-id actions follow the wiring guard: forms post to collection endpoints
// with the id in the body (`/web/customers/edit`, `/web/customers/activate`,
// `/web/customers/deactivate`, `/web/customers/delete`, `/web/customer-receipts`)
// because HTMX cannot interpolate a path segment from an input value.

use askama::Template;
use axum::{
    extract::{Extension, Form, Path, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::HashMap;

use crate::error::{AppError, AppResult};
use crate::localization::LocalizationContext;
use crate::models::{
    Ageing, Customer, CustomerStatement, NewCustomer, PaymentMethodWithAccount, PriceRefusal,
    ReceiptDetail, SaleListRow, SetMoney, UpdateCustomer,
};
use crate::routes::AppState;
use crate::security::authz::{CustomersCollect, CustomersRead, CustomersWrite, Nav, Require};

// S6 enforcement (AC10): the entity and its derived receivable are read with
// `customers.read`, the entity is mutated with `customers.write`, and money
// in — the collect form grouping invoices — is `customers.collect`.

// ---------------------------------------------------------------------------
// Views + Askama templates
// ---------------------------------------------------------------------------

/// One list row: the customer plus the derived receivable the routes compose.
#[derive(Clone)]
pub struct CustomerRow {
    pub customer: Customer,
    /// The customer's receivable, or a refusal when a document in it cannot be
    /// totaled — never a partial figure.
    pub balance: SetMoney,
    pub ageing: Ageing,
    pub over_limit: bool,
}

/// One ledger row of a statement, with each of its two derived figures already
/// resolved for the template: the amount where the arithmetic carried it, and the
/// refusal — in the operator's language, through the one shared mapping — where it
/// did not.
struct StatementEntryView {
    entry: crate::models::StatementEntry,
    /// The sentence for a refused debit. Empty when the debit is a figure.
    debit_message: String,
    /// The sentence for a refused running balance. Empty when it is a figure.
    balance_message: String,
}

/// One receivable row of the customer drawer: the document, plus the refusal
/// already in the operator's language.
///
/// The sentence is resolved HERE, through the one shared `price_refusal_key`
/// mapping, so this surface cannot word the rule differently from the record
/// pages, the sales list or the index.
struct DebtRowView {
    sale: crate::models::Sale,
    money: Option<crate::models::RecordMoney>,
    /// Empty when the document's money could be carried.
    total_refusal_message: String,
}

#[derive(Template)]
#[template(path = "customers.html")]
struct CustomersTemplate {
    title: String,
    localization: LocalizationContext,
    customers: Vec<CustomerRow>,
    statement: Option<CustomerStatement>,
    /// The statement's balance and ageing grid, resolved into the operator's
    /// language by `statement_figures`.
    statement_figures: StatementFigures,
    /// The ledger, one resolved row per entry.
    entries: Vec<StatementEntryView>,
    selected: Option<Customer>,
    receipts: Vec<ReceiptDetail>,
    debt_sales: Vec<DebtRowView>,
    method_options: Vec<PaymentMethodWithAccount>,
    today: String,
    warning: Option<String>,
    over_limit: bool,
    drawer_open: bool,
    /// Audit display names (M5 Phase B, slice S11): the selected customer's
    /// creator and last editor, resolved in this wiring layer.
    created_by_name: Option<String>,
    updated_by_name: Option<String>,
    nav_key: &'static str,
    /// The sidebar's nav view: the entries this principal may read (S7 part 2).
    nav: Nav,
}

#[derive(Template)]
#[template(path = "partials/customer_list.html")]
struct CustomerListPartial {
    customers: Vec<CustomerRow>,
    warning: Option<String>,
    localization: LocalizationContext,
}

/// Every figure the statement partial renders, already resolved into the
/// operator's language: the amount, or the sentence in the place the figure
/// would be.
///
/// The buckets are resolved HERE and not in the template, so the template has no
/// way to print a number where a sum was refused — the grid reads seven strings,
/// and a refused bucket's string IS the sentence. The total travels beside them
/// because a total can refuse for a reason no single bucket can show: the buckets
/// are a partition, and their sum is a second set sum (see [`Ageing::total`]).
#[derive(Clone, Default)]
pub struct StatementFigures {
    pub balance: String,
    pub ageing_total: String,
    pub ageing_totalled: bool,
    pub current: String,
    pub overdue_1_30: String,
    pub overdue_31_60: String,
    pub overdue_61_plus: String,
}

/// The slide-over drawer body: the statement (header, balance, ageing,
/// documents) plus the collect form and the payment history. The field names
/// mirror `CustomersTemplate` so `customers.html` can include the same
/// partial it renders for a direct `/customers/{id}` visit.
#[derive(Template)]
#[template(path = "partials/customer_detail.html")]
struct CustomerDetailPartial {
    localization: LocalizationContext,
    statement: Option<CustomerStatement>,
    /// The statement's balance and ageing grid, resolved into the operator's
    /// language by `statement_figures`.
    statement_figures: StatementFigures,
    entries: Vec<StatementEntryView>,
    selected: Option<Customer>,
    debt_sales: Vec<DebtRowView>,
    receipts: Vec<ReceiptDetail>,
    method_options: Vec<PaymentMethodWithAccount>,
    today: String,
    over_limit: bool,
    /// Audit display names: the customer's creator and its last editor.
    created_by_name: Option<String>,
    updated_by_name: Option<String>,
}

#[derive(Template)]
#[template(path = "partials/receipt_list.html")]
struct ReceiptListPartial {
    localization: LocalizationContext,
    receipts: Vec<ReceiptDetail>,
}

/// Prefilled edit form for the row-level ✎ button: the list rows carry only
/// the name, so this read-only fragment loads the entity's data into the
/// `<dialog>` modal. It posts to the existing `/web/customers/edit` backend.
#[derive(Template)]
#[template(path = "partials/customer_edit_form.html")]
struct CustomerEditFormPartial {
    customer: Customer,
    localization: LocalizationContext,
}

// ---------------------------------------------------------------------------
// Helpers (mirror sales_web / suppliers_web)
// ---------------------------------------------------------------------------

fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .map(|v| v == "true")
        .unwrap_or(false)
}

fn today(localization: &LocalizationContext) -> AppResult<NaiveDate> {
    localization
        .today_iso()
        .parse()
        .map_err(|_| AppError::Internal("invalid localized date".into()))
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

fn parse_date_or_today(s: &str, localization: &LocalizationContext) -> AppResult<NaiveDate> {
    let t = s.trim();
    if t.is_empty() {
        return today(localization);
    }
    t.parse()
        .map_err(|_| AppError::Validation("invalid date (YYYY-MM-DD)".into()))
}

fn clean_opt(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// `credit_limit` is nullable: null means no limit, so the flag can never block.
/// The statement's derived figures, resolved for the template: the amount where
/// the receivable totals, and the refusal — in the operator's language, through
/// the one shared mapping — where it does not.
///
/// Both are strings rather than a model the template branches on, so the page has
/// exactly one place to render the rule and the route is the only layer that
/// knows the locale. `ageing_totalled` says which of the two the page shows.
fn statement_entry_views(
    statement: &CustomerStatement,
    localization: &LocalizationContext,
) -> Vec<StatementEntryView> {
    statement
        .entries
        .iter()
        .map(|entry| {
            let message = |refusal: Option<PriceRefusal>| {
                refusal
                    .map(|refusal| crate::routes::price_refusal_message(&refusal, localization))
                    .unwrap_or_default()
            };
            StatementEntryView {
                debit_message: message(entry.debit.refusal),
                balance_message: message(entry.balance.refusal),
                entry: entry.clone(),
            }
        })
        .collect()
}

fn statement_figures(
    statement: &CustomerStatement,
    localization: &LocalizationContext,
) -> StatementFigures {
    let sentence =
        |refusal: PriceRefusal| crate::routes::price_refusal_message(&refusal, localization);
    let figure = |money: SetMoney| match money.amount {
        Some(amount) => localization.format_currency(amount),
        None => money.refusal.map(sentence).unwrap_or_default(),
    };
    let total = statement.ageing.total();
    let [current, overdue_1_30, overdue_31_60, overdue_61_plus] = statement.ageing.buckets();
    StatementFigures {
        balance: figure(statement.balance),
        ageing_total: figure(total),
        // Whether the TOTAL is a figure. The grid renders either way: a bucket
        // that carried keeps its own sum, and a bucket that refused says so in
        // its own cell. Hiding the whole grid would throw away three real sums
        // because of a fourth.
        ageing_totalled: total.amount.is_some(),
        current: figure(current),
        overdue_1_30: figure(overdue_1_30),
        overdue_31_60: figure(overdue_31_60),
        overdue_61_plus: figure(overdue_61_plus),
    }
}

fn debt_row_views(rows: Vec<SaleListRow>, localization: &LocalizationContext) -> Vec<DebtRowView> {
    rows.into_iter()
        .map(|row| {
            let total_refusal_message = row
                .total_refusal
                .map(|refusal| crate::routes::price_refusal_message(&refusal, localization))
                .unwrap_or_default();
            DebtRowView {
                sale: row.sale,
                money: row.money,
                total_refusal_message,
            }
        })
        .collect()
}

fn over_limit(customer: &Customer, balance: SetMoney) -> bool {
    match (customer.credit_limit, balance.amount) {
        (Some(limit), Some(amount)) => amount > limit,
        // A refused figure makes no claim either way: the comparison needs a
        // number that does not exist, and the refusal is stated where the balance
        // was, so the absence of the flag is explained on the page.
        _ => false,
    }
}

/// Resolve the selected customer's audit display names in this wiring layer
/// (AC20: the department never reads identity tables; the resolution lives in
/// the routes' shared helper).
async fn customer_actor_names(
    state: &AppState,
    customer: &Customer,
) -> AppResult<(Option<String>, Option<String>)> {
    let mut actor_ids = vec![customer.created_by];
    actor_ids.extend(customer.updated_by);
    let names = crate::routes::audit_actor_names(&state.pool, &actor_ids).await?;
    let created = names.get(&customer.created_by).cloned();
    let updated = customer.updated_by.and_then(|id| names.get(&id).cloned());
    Ok((created, updated))
}

/// Every active/inactive customer with the derived receivable folded in.
/// `ageing_all` only returns customers with a non-zero balance, so absent
/// entries are a zero balance with an empty ageing.
async fn customer_rows(
    state: &AppState,
    localization: &LocalizationContext,
) -> AppResult<Vec<CustomerRow>> {
    let as_of = today(localization)?;
    let ageings: HashMap<i64, (SetMoney, Ageing)> = state
        .sales_service
        .ageing_all(as_of)
        .await?
        .into_iter()
        .map(|row| (row.customer_id, (row.balance, row.ageing)))
        .collect();
    let customers = state.customer_service.list_customers(false).await?;
    Ok(customers
        .into_iter()
        .map(|customer| {
            let (balance, ageing) = ageings
                .get(&customer.id)
                .copied()
                .unwrap_or((SetMoney::amount(Decimal::ZERO), Ageing::default()));
            let over_limit = over_limit(&customer, balance);
            CustomerRow {
                customer,
                balance,
                ageing,
                over_limit,
            }
        })
        .collect())
}

fn render_list(
    customers: Vec<CustomerRow>,
    warning: Option<String>,
    localization: &LocalizationContext,
) -> AppResult<Html<String>> {
    let html = CustomerListPartial {
        customers,
        warning,
        localization: localization.clone(),
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

async fn list_response(
    state: &AppState,
    warning: Option<String>,
    localization: &LocalizationContext,
) -> AppResult<Response> {
    let rows = customer_rows(state, localization).await?;
    Ok(render_list(rows, warning, localization)?.into_response())
}

// ---------------------------------------------------------------------------
// Pages + fragments
// ---------------------------------------------------------------------------

async fn customers_page(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    Extension(localization): Extension<LocalizationContext>,
    principal: axum::Extension<crate::security::authz::Principal>,
) -> Result<Html<String>, AppError> {
    let customers = customer_rows(&state, &localization).await?;
    let tmpl = CustomersTemplate {
        title: format!(
            "Roya — {}",
            localization.tr(crate::localization::MessageKey::CustomerTitle)
        ),
        localization,
        customers,
        statement: None,
        // No statement on the list page: there is no receivable to state and no
        // refusal to state with it, so both strings are empty and the template's
        // `if let Some(st)` guard means neither is read.
        statement_figures: StatementFigures::default(),
        entries: vec![],
        selected: None,
        receipts: vec![],
        debt_sales: vec![],
        method_options: vec![],
        today: String::new(),
        warning: None,
        over_limit: false,
        drawer_open: false,
        created_by_name: None,
        updated_by_name: None,
        nav_key: "customers",
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(
        tmpl.render()
            .map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

// The statement page is the customer's own account view (statement,
// documents, receipts): the module owns it, so the single gate is
// `customers.read` — see the note on `customer_statement` in `customers_api.rs`.
async fn customer_statement_page(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    Extension(localization): Extension<LocalizationContext>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
) -> Result<Html<String>, AppError> {
    let customer = state.customer_service.get_customer(id).await?;
    let as_of = today(&localization)?;
    let statement = state.sales_service.customer_statement(id, as_of).await?;
    let over_limit = over_limit(&customer, statement.balance);
    let statement_figures = statement_figures(&statement, &localization);
    let entries = statement_entry_views(&statement, &localization);
    let debt_sales = debt_row_views(
        state.sales_service.customer_debt_sales(id).await?,
        &localization,
    );
    let receipts = state.customer_receipt_service.list_receipts(id).await?;
    let method_options = state.payment_method_service.methods_with_accounts().await?;
    let (created_by_name, updated_by_name) = customer_actor_names(&state, &customer).await?;
    let tmpl = CustomersTemplate {
        title: format!(
            "Roya — {}: {}",
            localization.tr(crate::localization::MessageKey::CustomerTitle),
            customer.name
        ),
        localization: localization.clone(),
        customers: customer_rows(&state, &localization).await?,
        statement: Some(statement),
        statement_figures,
        entries,
        selected: Some(customer),
        receipts,
        debt_sales,
        method_options,
        today: as_of.to_string(),
        warning: None,
        over_limit,
        drawer_open: true,
        created_by_name,
        updated_by_name,
        nav_key: "customers",
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(
        tmpl.render()
            .map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

async fn web_customer_list(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    Extension(localization): Extension<LocalizationContext>,
) -> AppResult<Response> {
    list_response(&state, None, &localization).await
}

async fn web_customer_detail(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    detail_html(&state, id, localization).await
}

async fn web_customer_edit_form(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let customer = state.customer_service.get_customer(id).await?;
    let html = CustomerEditFormPartial {
        customer,
        localization,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

/// The drawer body with fresh derived data: statement, documents, receipts
/// and the collect context. Both the detail fragment and the collect action
/// answer it, so collecting refreshes the balance in place without the client
/// rebuilding a URL.
async fn detail_html(
    state: &AppState,
    id: i64,
    localization: LocalizationContext,
) -> AppResult<Html<String>> {
    let customer = state.customer_service.get_customer(id).await?;
    let as_of = today(&localization)?;
    let statement = state.sales_service.customer_statement(id, as_of).await?;
    let over_limit = over_limit(&customer, statement.balance);
    let statement_figures = statement_figures(&statement, &localization);
    let entries = statement_entry_views(&statement, &localization);
    let debt_sales = debt_row_views(
        state.sales_service.customer_debt_sales(id).await?,
        &localization,
    );
    let receipts = state.customer_receipt_service.list_receipts(id).await?;
    let method_options = state.payment_method_service.methods_with_accounts().await?;
    let (created_by_name, updated_by_name) = customer_actor_names(&state, &customer).await?;
    let html = CustomerDetailPartial {
        localization: localization.clone(),
        statement: Some(statement),
        statement_figures,
        entries,
        selected: Some(customer),
        debt_sales,
        receipts,
        method_options,
        today: as_of.to_string(),
        over_limit,
        created_by_name,
        updated_by_name,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

async fn web_customer_receipts(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let receipts = state.customer_receipt_service.list_receipts(id).await?;
    let html = ReceiptListPartial {
        localization,
        receipts,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

// ---------------------------------------------------------------------------
// Forms (HTMX, collection endpoints with the typed id in the body)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct CreateCustomerForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub tax_id: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub credit_limit: String,
    #[serde(default)]
    pub due_days: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct EditCustomerForm {
    #[serde(default)]
    pub customer_id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub tax_id: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub credit_limit: String,
    #[serde(default)]
    pub due_days: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct CustomerIdForm {
    #[serde(default)]
    pub customer_id: i64,
}

#[derive(Debug, Deserialize, Default)]
pub struct CollectForm {
    #[serde(default)]
    pub customer_id: i64,
    #[serde(default)]
    pub method_id: i64,
    #[serde(default)]
    pub amount: String,
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub notes: String,
}

async fn web_create_customer(
    State(state): State<AppState>,
    _: Require<CustomersWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<CreateCustomerForm>,
) -> AppResult<Response> {
    let result = state
        .customer_service
        .create_customer(
            principal.user_id,
            NewCustomer {
                name: form.name,
                phone: clean_opt(&form.phone),
                address: clean_opt(&form.address),
                tax_id: clean_opt(&form.tax_id),
                notes: clean_opt(&form.notes),
                is_walkin: false,
                credit_limit: parse_opt_decimal(&form.credit_limit, "credit_limit", &localization)?,
                due_days: parse_opt_i64(&form.due_days, "due_days")?,
            },
        )
        .await?;
    // The name is not unique: report the existing matches as a warning, never a
    // rejection (AC15).
    let warning = if result.name_matches.is_empty() {
        None
    } else {
        let matches: Vec<String> = result
            .name_matches
            .iter()
            .map(|c| format!("#{} {}", c.id, c.name))
            .collect();
        Some(localization.tr_with(
            crate::localization::MessageKey::CustomerDuplicateWarning,
            &[
                ("name", &result.customer.name),
                ("matches", &matches.join(", ")),
            ],
        ))
    };
    if is_htmx(&headers) {
        let mut resp = list_response(&state, warning, &localization).await?;
        resp.headers_mut()
            .insert("HX-Trigger", "customer-created".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/customers").into_response())
}

async fn web_update_customer(
    State(state): State<AppState>,
    _: Require<CustomersWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<EditCustomerForm>,
) -> AppResult<Response> {
    // The form replaces the editable fields: empty optional values clear, an
    // empty name is rejected by the service.
    state
        .customer_service
        .update_customer(
            form.customer_id,
            principal.user_id,
            UpdateCustomer {
                name: Some(form.name),
                phone: Some(clean_opt(&form.phone)),
                address: Some(clean_opt(&form.address)),
                tax_id: Some(clean_opt(&form.tax_id)),
                notes: Some(clean_opt(&form.notes)),
                credit_limit: Some(parse_opt_decimal(
                    &form.credit_limit,
                    "credit_limit",
                    &localization,
                )?),
                due_days: Some(parse_opt_i64(&form.due_days, "due_days")?),
            },
        )
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(&state, None, &localization).await?;
        resp.headers_mut()
            .insert("HX-Trigger", "customer-changed".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/customers").into_response())
}

async fn web_activate_customer(
    State(state): State<AppState>,
    _: Require<CustomersWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<CustomerIdForm>,
) -> AppResult<Response> {
    state
        .customer_service
        .activate_customer(principal.user_id, form.customer_id)
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(&state, None, &localization).await?;
        resp.headers_mut()
            .insert("HX-Trigger", "customer-changed".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/customers").into_response())
}

async fn web_deactivate_customer(
    State(state): State<AppState>,
    _: Require<CustomersWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<CustomerIdForm>,
) -> AppResult<Response> {
    state
        .customer_service
        .deactivate_customer(principal.user_id, form.customer_id)
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(&state, None, &localization).await?;
        resp.headers_mut()
            .insert("HX-Trigger", "customer-changed".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/customers").into_response())
}

async fn web_delete_customer(
    State(state): State<AppState>,
    _: Require<CustomersWrite>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<CustomerIdForm>,
) -> AppResult<Response> {
    state
        .customer_service
        .delete_customer(form.customer_id)
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(&state, None, &localization).await?;
        resp.headers_mut()
            .insert("HX-Trigger", "customer-changed".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/customers").into_response())
}

/// Collect through the collection endpoint: the receipt is derived from the
/// customer in the body and applied oldest-first, so the form never names a
/// receipt id.
async fn web_collect_receipt(
    State(state): State<AppState>,
    _: Require<CustomersCollect>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<CollectForm>,
) -> AppResult<Response> {
    let amount = parse_required_decimal(&form.amount, "amount", &localization)?;
    let date = parse_date_or_today(&form.date, &localization)?;
    state
        .customer_receipt_service
        .collect(
            principal.user_id,
            form.customer_id,
            form.method_id,
            amount,
            date,
            clean_opt(&form.notes),
        )
        .await?;
    if is_htmx(&headers) {
        return Ok(detail_html(&state, form.customer_id, localization)
            .await?
            .into_response());
    }
    Ok(Redirect::to(&format!("/customers/{}", form.customer_id)).into_response())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/customers", get(customers_page))
        .route("/customers/{id}", get(customer_statement_page))
        .route(
            "/web/customers",
            get(web_customer_list).post(web_create_customer),
        )
        .route("/web/customers/edit", post(web_update_customer))
        .route("/web/customers/activate", post(web_activate_customer))
        .route("/web/customers/deactivate", post(web_deactivate_customer))
        .route("/web/customers/delete", post(web_delete_customer))
        .route("/web/customers/detail/{id}", get(web_customer_detail))
        .route("/web/customers/edit-form/{id}", get(web_customer_edit_form))
        .route("/web/customers/{id}/receipts", get(web_customer_receipts))
        .route("/web/customer-receipts", post(web_collect_receipt))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use chrono::NaiveDate;
    use rust_decimal::Decimal;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    use tower::ServiceExt;

    use crate::models::{
        MovementReason, MovementType, NewCustomer, NewMovement, NewProduct, NewSale, PaymentType,
        ProductKind,
    };
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
            .foreign_keys(true)
            .pragma("recursive_triggers", "1");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        // S1b part 1: seed the fixed test session every request will authenticate with.
        test_support::seed_session(&pool).await.unwrap();
        // Enforcement off: the fixture confirms an over-limit sale and the page
        // must report the customer as over limit (the 400 path is covered by the
        // REST and smoke suites).
        AppState::new_with_credit_limit(pool, false, true, false)
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

    struct WebFixture {
        customer: i64,
        account: i64,
        cash: i64,
        product: i64,
        sale: i64,
    }

    /// One customer with a 75 credit debt (3 × 25) and the account/method pair
    /// the collect form uses.
    async fn seed_fixture(state: &AppState) -> WebFixture {
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
            .expect("Cash is seeded")
            .id;
        let product = state
            .inventory_service
            .create_product(
                audit_actor(&state).await,
                NewProduct {
                    sku: "WEB-CUST-P".into(),
                    name: "prod WEB-CUST-P".into(),
                    kind: ProductKind::Product,
                    category_id: None,
                    unit: "un".into(),
                    sale_price: Decimal::from(25),
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
                    qty: Decimal::from(100),
                    movement_type: MovementType::In,
                    reason: MovementReason::Initial,
                    reference: String::new(),
                    date: NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
                },
            )
            .await
            .unwrap();
        let customer = state
            .customer_service
            .create_customer(
                audit_actor(&state).await,
                NewCustomer {
                    name: "Ana Web".into(),
                    phone: None,
                    address: None,
                    tax_id: None,
                    notes: None,
                    is_walkin: false,
                    credit_limit: Some(Decimal::from(40)),
                    due_days: Some(30),
                },
            )
            .await
            .unwrap()
            .customer;
        let sale = state
            .sales_service
            .create_draft(
                audit_actor(&state).await,
                NewSale {
                    customer_id: customer.id,
                    payment_type: PaymentType::Credit,
                    sale_date: NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                    due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 15).unwrap()),
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
                Decimal::from(3),
                None,
            )
            .await
            .unwrap();
        state
            .sales_service
            .confirm(audit_actor(&state).await, sale.id, None)
            .await
            .unwrap();
        WebFixture {
            customer: customer.id,
            account: account.id,
            cash,
            product: product.id,
            sale: sale.id,
        }
    }

    /// A confirmed credit sale of `4e28` — one document that TOTALS on its own,
    /// so every refusal below is a statement about the SET of documents and never
    /// about a line.
    async fn huge_credit_sale(
        state: &AppState,
        customer_id: i64,
        product_id: i64,
        due_date: NaiveDate,
    ) -> i64 {
        let sale = state
            .sales_service
            .create_draft(
                audit_actor(state).await,
                NewSale {
                    customer_id,
                    payment_type: PaymentType::Credit,
                    sale_date: due_date,
                    due_date: Some(due_date),
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
                product_id,
                Decimal::ONE,
                Some(Decimal::from_str("4e28").unwrap()),
            )
            .await
            .unwrap();
        state
            .sales_service
            .confirm(audit_actor(state).await, sale.id, None)
            .await
            .unwrap();
        sale.id
    }

    /// F1: the ageing buckets are a PARTITION of one receivable, and a bound that
    /// holds inside each bucket says nothing about the sum ACROSS them. Two
    /// documents of `4e28` that land in DIFFERENT buckets each fit their own
    /// bucket, and their sum is `8e28`, which `Decimal` cannot carry: the page
    /// must state the rule instead of overflowing.
    #[tokio::test]
    async fn a_customer_whose_documents_land_in_different_ageing_buckets_states_the_rule() {
        let state = test_state().await;
        let fixture = seed_fixture(&state).await;
        let today = chrono::Local::now().date_naive();
        // One not yet due (`current`) and one ten days late (`overdue_1_30`): the
        // two buckets that a fixed `as_of` splits them into.
        huge_credit_sale(
            &state,
            fixture.customer,
            fixture.product,
            today + chrono::Days::new(10),
        )
        .await;
        huge_credit_sale(
            &state,
            fixture.customer,
            fixture.product,
            today - chrono::Days::new(10),
        )
        .await;
        let app = crate::routes::router(state);

        let (status, html) = get_html(app, &format!("/customers/{}", fixture.customer)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("data-document-total-refusal"),
            "the ageing total refuses instead of overflowing: {html:.400}"
        );
    }

    #[tokio::test]
    async fn seeded_payment_method_labels_are_bilingual_and_ids_stay_canonical() {
        let state = test_state().await;
        let fixture = seed_fixture(&state).await;
        let app = crate::routes::router(state.clone());

        set_locale(&state, "en-US", "en").await;
        let (status, html) = get_html(
            app.clone(),
            &format!("/web/customers/detail/{}", fixture.customer),
        )
        .await;
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
            html.contains(&format!("value=\"{}\"", fixture.cash)),
            "the canonical method id must remain unchanged: {html:.1200}"
        );

        set_locale(&state, "es-ES", "es").await;
        let (status, html) =
            get_html(app, &format!("/web/customers/detail/{}", fixture.customer)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Efectivo — Caja"), "{html:.1200}");
        assert!(
            !html.contains("sin asignar"),
            "the Spanish catalog must not offer an ownerless method either: {html:.1200}"
        );
    }

    #[tokio::test]
    async fn web_customers_page_is_names_only_with_drawer_and_modal() {
        let state = test_state().await;
        let fixture = seed_fixture(&state).await;
        let app = crate::routes::router(state);

        // The list page shows names only: a New customer button opens the
        // modal, a hidden drawer waits for the detail, and no edit card or
        // list-level metrics leak through.
        let (status, html) = get_html(app.clone(), "/customers").await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        for expected in [
            "Ana Web",
            "New customer",
            "<dialog",
            "new-customer-dialog",
            "customer-drawer",
            "customer-drawer-body",
        ] {
            assert!(
                html.contains(expected),
                "page must show {expected}: {html:.600}"
            );
        }
        for absent in ["Edit Customer", "over limit", "1-30", "31-60", "61+"] {
            assert!(
                !html.contains(absent),
                "names-only page must not show {absent}: {html:.600}"
            );
        }
        // Each name loads its detail fragment into the drawer.
        assert!(
            html.contains(&format!(
                "hx-get=\"/web/customers/detail/{}\"",
                fixture.customer
            )),
            "names must open the drawer through the detail fragment: {html:.600}"
        );

        let (status, html) = get_html(app.clone(), "/web/customers").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Ana Web"), "{html:.400}");
        assert!(
            !html.contains("over limit"),
            "the names-only fragment keeps no badges: {html:.400}"
        );

        // The statement page keeps the customer context and renders the drawer
        // open with its fragments.
        let (status, html) =
            get_html(app.clone(), &format!("/customers/{}", fixture.customer)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        for expected in [
            "Ana Web",
            "Ageing",
            "Receivable sales",
            "75",
            "Over limit",
            "Collect",
            "Payment history",
        ] {
            assert!(
                html.contains(expected),
                "statement must show {expected}: {html:.600}"
            );
        }
        assert!(
            html.contains(&format!("/web/customers/detail/{}", fixture.customer)),
            "statement page must refresh through its fragment endpoint"
        );

        // The detail fragment carries the header, balance, ageing buckets, the
        // over-limit marker and the collect form for the drawer.
        let (status, html) = get_html(
            app.clone(),
            &format!("/web/customers/detail/{}", fixture.customer),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        for expected in [
            "Ana Web",
            "Current",
            "1–30",
            "75",
            "Over limit",
            "Collect",
            "Payment history",
        ] {
            assert!(
                html.contains(expected),
                "detail must show {expected}: {html:.600}"
            );
        }

        let (status, html) = get_html(
            app,
            &format!("/web/customers/{}/receipts", fixture.customer),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("No receipts"), "{html:.400}");
    }

    #[tokio::test]
    async fn web_create_edit_and_warn_on_duplicate() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());

        let (status, html) = post_form(
            app.clone(),
            "/web/customers",
            "name=Juan+P%C3%A9rez&phone=555-1234&credit_limit=100&due_days=30",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(html.contains("Juan Pérez"), "{html:.400}");

        // The names-only list carries no credit limit; the limit renders in
        // the drawer detail fragment instead.
        let (row,): (i64,) = sqlx::query_as("SELECT id FROM customers WHERE name = 'Juan Pérez'")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let (status, html) = get_html(app.clone(), &format!("/web/customers/detail/{row}")).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(html.contains("100"), "{html:.400}");

        // A duplicate name is accepted and the existing match is reported.
        let (status, html) = post_form(app.clone(), "/web/customers", "name=Juan+P%C3%A9rez").await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("already exists"),
            "the duplicate must be reported as a warning: {html:.600}"
        );

        let (row,): (i64,) = sqlx::query_as("SELECT id FROM customers WHERE name = 'Juan Pérez'")
            .fetch_one(&state.pool)
            .await
            .unwrap();

        // Edit replaces the fields; empty optional fields clear.
        let (status, html) = post_form(
            app.clone(),
            "/web/customers/edit",
            &format!("customer_id={row}&name=Juan+P.&phone=&credit_limit=&due_days=7"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(html.contains("Juan P."), "{html:.400}");
        let stored = state.customer_service.get_customer(row).await.unwrap();
        assert_eq!(stored.name, "Juan P.");
        assert_eq!(stored.phone, None);
        assert_eq!(stored.credit_limit, None);
        assert_eq!(stored.due_days, Some(7));

        // An empty name is rejected and changes nothing.
        let (status, body) = post_form(
            app.clone(),
            "/web/customers/edit",
            &format!("customer_id={row}&name=&due_days="),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            state.customer_service.get_customer(row).await.unwrap().name,
            "Juan P."
        );

        // Toggle through the collection endpoints with the id in the body.
        let (status, html) = post_form(
            app.clone(),
            "/web/customers/deactivate",
            &format!("customer_id={row}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            !state
                .customer_service
                .get_customer(row)
                .await
                .unwrap()
                .is_active
        );
        let (status, _) = post_form(
            app.clone(),
            "/web/customers/activate",
            &format!("customer_id={row}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            state
                .customer_service
                .get_customer(row)
                .await
                .unwrap()
                .is_active
        );

        // Delete without history.
        let (status, _) = post_form(
            app.clone(),
            "/web/customers/delete",
            &format!("customer_id={row}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(state.customer_service.get_customer(row).await.is_err());

        // The seeded walk-in is refused with a clean 400.
        let (walkin,): (i64,) = sqlx::query_as("SELECT id FROM customers WHERE is_walkin = 1")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let (status, body) = post_form(
            app,
            "/web/customers/deactivate",
            &format!("customer_id={walkin}"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    #[tokio::test]
    async fn web_collect_form_applies_oldest_first_and_updates_statement() {
        let state = test_state().await;
        let fixture = seed_fixture(&state).await;
        let app = crate::routes::router(state.clone());

        // The rendered collect form carries the customer, amount and method
        // (the account is derived from the method), and posts to the
        // collection endpoint with the id in the body.
        let (status, html) =
            get_html(app.clone(), &format!("/customers/{}", fixture.customer)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        let form_start = html
            .find("hx-post=\"/web/customer-receipts\"")
            .expect("the collect form must post to the collection endpoint");
        let form = &html[form_start..];
        for field in [
            "name=\"customer_id\"",
            "name=\"amount\"",
            "name=\"method_id\"",
        ] {
            assert!(
                form.contains(field),
                "collect form must carry {field}: {form:.600}"
            );
        }
        assert!(
            !form.contains("name=\"account_id\""),
            "the collect form must not ask for an account: {form:.600}"
        );

        // Collect 30 of the 75 debt.
        let (status, html) = post_form(
            app.clone(),
            "/web/customer-receipts",
            &format!(
                "customer_id={}&method_id={}&amount=30&date=2024-06-20&notes=part",
                fixture.customer, fixture.cash
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("30"),
            "the refreshed receipts must show it: {html:.600}"
        );

        // The statement fragment now mixes the sale debit with the payment credit.
        let (status, html) = get_html(
            app.clone(),
            &format!("/web/customers/detail/{}", fixture.customer),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Payment"), "{html:.600}");

        // The page reads the derived 45 balance.
        let (status, html) =
            get_html(app.clone(), &format!("/customers/{}", fixture.customer)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("45"),
            "derived balance after collecting: {html:.600}"
        );

        // Over-collecting is refused and the refreshed list is untouched.
        let (status, body) = post_form(
            app.clone(),
            "/web/customer-receipts",
            &format!(
                "customer_id={}&method_id={}&amount=1000&date=2024-06-20",
                fixture.customer, fixture.cash
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            state
                .customer_receipt_service
                .list_receipts(fixture.customer)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn web_customer_forms_wire_collection_endpoints() {
        let state = test_state().await;
        let fixture = seed_fixture(&state).await;
        let app = crate::routes::router(state);

        // The list page carries only the modal create form: no edit card, no
        // per-row buttons, and a drawer container waiting for the detail.
        let (status, html) = get_html(app.clone(), "/customers").await;
        assert_eq!(status, StatusCode::OK);

        let mut targets = Vec::new();
        let mut rest = html.as_str();
        while let Some(start) = rest.find("hx-post=\"") {
            let after = &rest[start + "hx-post=\"".len()..];
            let end = after.find('"').expect("unterminated hx-post attribute");
            targets.push(after[..end].to_string());
            rest = &after[end..];
        }
        assert!(
            targets.iter().any(|t| t == "/web/customers"),
            "customers page must create through the collection endpoint: {targets:?}"
        );
        assert!(
            !targets.iter().any(|t| t == "/web/customers/edit"),
            "no visible edit card may post to the edit endpoint: {targets:?}"
        );
        for target in &targets {
            assert!(
                !target.contains("/0/"),
                "collection endpoints must not hardcode an id: {target}"
            );
        }
        assert!(
            !html.contains("this.action="),
            "dead onsubmit action rewrite still rendered"
        );
        assert!(
            html.contains("id=\"customer-drawer\""),
            "customers page must carry the detail drawer"
        );
        assert!(
            html.contains("<dialog"),
            "customers page must create through a modal dialog"
        );

        // The detail fragment collects through the collection endpoint.
        let (status, html) =
            get_html(app, &format!("/web/customers/detail/{}", fixture.customer)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("hx-post=\"/web/customer-receipts\""),
            "detail fragment must collect through the collection endpoint"
        );
    }

    #[tokio::test]
    async fn web_customer_edit_form_is_prefilled_and_posts_to_edit() {
        let state = test_state().await;
        let fixture = seed_fixture(&state).await;
        let app = crate::routes::router(state);

        // Each list row carries the ✎ button loading this fragment, next to
        // the name button opening the drawer: a div row with two buttons,
        // never a nested button.
        let (status, html) = get_html(app.clone(), "/web/customers").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains(&format!(
                "hx-get=\"/web/customers/edit-form/{}\"",
                fixture.customer
            )),
            "rows must offer the edit form: {html:.600}"
        );
        assert!(
            html.contains("hx-get=\"/web/customers/detail/"),
            "the name button must keep opening the drawer: {html:.600}"
        );

        // The fragment prefills the entity's data and posts to the existing
        // edit backend with the id in the body.
        let (status, html) = get_html(
            app.clone(),
            &format!("/web/customers/edit-form/{}", fixture.customer),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        for expected in [
            "hx-post=\"/web/customers/edit\"",
            &format!("name=\"customer_id\" value=\"{}\"", fixture.customer),
            "value=\"Ana Web\"",
            "name=\"credit_limit\"",
            "name=\"due_days\"",
        ] {
            assert!(
                html.contains(expected),
                "edit form must show {expected}: {html:.600}"
            );
        }

        let (status, _) = get_html(app, "/web/customers/edit-form/999999").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
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

    /// A principal holding ONLY `customers.read` opens the reads and is
    /// refused every web mutation, each naming its own code.
    #[tokio::test]
    async fn ac10_a_customers_read_only_principal_is_refused_the_web_mutations() {
        let state = test_state().await;
        let fixture = seed_fixture(&state).await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["customers.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        // The reads the probe is allowed: the page, the statement page, the
        // fragments and the edit form.
        for uri in [
            "/customers".to_string(),
            format!("/customers/{}", fixture.customer),
            "/web/customers".to_string(),
            format!("/web/customers/detail/{}", fixture.customer),
            format!("/web/customers/{}/receipts", fixture.customer),
            format!("/web/customers/edit-form/{}", fixture.customer),
        ] {
            let (status, html) = get_html_as(app.clone(), &uri, Some(&cookie)).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {html:.200}");
        }

        // Entity mutations over HTMX: the JSON refusal naming customers.write.
        for (uri, body) in [
            ("/web/customers", "name=Denied+Write"),
            (
                "/web/customers/edit",
                &format!("customer_id={}&name=Hacked", fixture.customer),
            ),
            (
                "/web/customers/activate",
                &format!("customer_id={}", fixture.customer),
            ),
            (
                "/web/customers/deactivate",
                &format!("customer_id={}", fixture.customer),
            ),
            (
                "/web/customers/delete",
                &format!("customer_id={}", fixture.customer),
            ),
        ] {
            let (status, body) = post_form_as(
                app.clone(),
                uri,
                body,
                &[("HX-Request", "true")],
                Some(&cookie),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
            assert!(
                body.contains("customers.write"),
                "{uri} must name customers.write: {body}"
            );
        }

        // The collect form is its own tier: customers.collect.
        let (status, body) = post_form_as(
            app,
            "/web/customer-receipts",
            &format!(
                "customer_id={}&method_id={}&amount=10&date=2024-05-03",
                fixture.customer, fixture.cash
            ),
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(
            body.contains("customers.collect"),
            "the refusal must name customers.collect: {body}"
        );
    }

    /// The refusal writes nothing: a refused delete (full-page shape) leaves
    /// the customers table intact and a refused collect leaves no receipt row.
    #[tokio::test]
    async fn ac10_the_customers_web_refusal_writes_nothing() {
        let state = test_state().await;
        let fixture = seed_fixture(&state).await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["customers.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        let customers_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM customers")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let (status, body) = post_form_as(
            app.clone(),
            "/web/customers/delete",
            &format!("customer_id={}", fixture.customer),
            &[],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body:.300}");
        assert!(
            body.contains("Action not permitted"),
            "the refusal must use the English fallback: {body:.300}"
        );
        assert!(
            body.contains("customers.write"),
            "the refusal must name customers.write: {body:.300}"
        );
        let customers_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM customers")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(
            customers_after, customers_before,
            "a refused delete must write nothing"
        );

        let receipts_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM customer_receipts")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let (status, body) = post_form_as(
            app,
            "/web/customer-receipts",
            &format!(
                "customer_id={}&method_id={}&amount=10&date=2024-05-03",
                fixture.customer, fixture.cash
            ),
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let receipts_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM customer_receipts")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(
            receipts_after, receipts_before,
            "a refused collect must write nothing"
        );
    }

    /// A principal holding the permissions gets the normal answers: the
    /// creation answers its HTMX list and the collect answers the detail.
    #[tokio::test]
    async fn ac10_the_customers_web_holding_principal_gets_the_normal_answer() {
        let state = test_state().await;
        let fixture = seed_fixture(&state).await;
        let holder = test_support::seed_session_with_permissions(
            &state.pool,
            &["customers.read", "customers.write", "customers.collect"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&holder);
        let app = crate::routes::router(state.clone());

        let (status, body) = post_form_as(
            app.clone(),
            "/web/customers",
            "name=Beto+Holder",
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.300}");
        assert!(
            body.contains("Beto Holder"),
            "the list must include the new customer"
        );

        let (status, body) = post_form_as(
            app,
            "/web/customer-receipts",
            &format!(
                "customer_id={}&method_id={}&amount=10&date=2024-05-03",
                fixture.customer, fixture.cash
            ),
            &[("HX-Request", "true")],
            Some(&cookie),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.300}");
        assert!(body.contains("Ana Web"), "the detail fragment must answer");
    }

    /// The read gates are real too: a principal WITHOUT `customers.read` (it
    /// holds an unrelated permission, so this is not a broken fixture) is
    /// refused every customers page and fragment with the full-page refusal
    /// card.
    #[tokio::test]
    async fn the_read_gates_refuse_a_principal_without_the_read_permission() {
        let state = test_state().await;
        let fixture = seed_fixture(&state).await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["sales.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state);

        for uri in [
            "/customers",
            &format!("/customers/{}", fixture.customer),
            "/web/customers",
            &format!("/web/customers/detail/{}", fixture.customer),
            &format!("/web/customers/edit-form/{}", fixture.customer),
            &format!("/web/customers/{}/receipts", fixture.customer),
        ] {
            let (status, html) = get_html_as(app.clone(), &uri, Some(&cookie)).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {html:.200}");
            assert!(
                html.contains("Action not permitted") && html.contains("customers.read"),
                "{uri} must refuse naming customers.read: {html:.300}"
            );
        }
    }

    /// The gate order must not change: an anonymous request gets the login
    /// redirect, never the permission refusal.
    #[tokio::test]
    async fn an_anonymous_request_still_gets_the_login_gate_not_the_permission_refusal() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let (status, _) =
            post_form_as(app.clone(), "/web/customers", "name=Anonymous", &[], None).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let req = Request::builder()
            .method("GET")
            .uri("/customers")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    }
}
