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
    extract::{Form, Path, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{
    Ageing, Customer, CustomerStatement, NewCustomer, PaymentMethodWithAccount, ReceiptDetail,
    SaleDetail, UpdateCustomer,
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
    pub balance: Decimal,
    pub ageing: Ageing,
    pub over_limit: bool,
}

#[derive(Template)]
#[template(path = "customers.html")]
struct CustomersTemplate {
    title: String,
    customers: Vec<CustomerRow>,
    statement: Option<CustomerStatement>,
    selected: Option<Customer>,
    receipts: Vec<ReceiptDetail>,
    debt_sales: Vec<SaleDetail>,
    method_options: Vec<PaymentMethodWithAccount>,
    today: String,
    warning: Option<String>,
    over_limit: bool,
    drawer_open: bool,
    nav_key: &'static str,
    /// The sidebar's nav view: the entries this principal may read (S7 part 2).
    nav: Nav,
}

#[derive(Template)]
#[template(path = "partials/customer_list.html")]
struct CustomerListPartial {
    customers: Vec<CustomerRow>,
    warning: Option<String>,
}

/// The slide-over drawer body: the statement (header, balance, ageing,
/// documents) plus the collect form and the payment history. The field names
/// mirror `CustomersTemplate` so `customers.html` can include the same
/// partial it renders for a direct `/customers/{id}` visit.
#[derive(Template)]
#[template(path = "partials/customer_detail.html")]
struct CustomerDetailPartial {
    statement: Option<CustomerStatement>,
    selected: Option<Customer>,
    debt_sales: Vec<SaleDetail>,
    receipts: Vec<ReceiptDetail>,
    method_options: Vec<PaymentMethodWithAccount>,
    today: String,
    over_limit: bool,
}

#[derive(Template)]
#[template(path = "partials/receipt_list.html")]
struct ReceiptListPartial {
    receipts: Vec<ReceiptDetail>,
}

/// Prefilled edit form for the row-level ✎ button: the list rows carry only
/// the name, so this read-only fragment loads the entity's data into the
/// `<dialog>` modal. It posts to the existing `/web/customers/edit` backend.
#[derive(Template)]
#[template(path = "partials/customer_edit_form.html")]
struct CustomerEditFormPartial {
    customer: Customer,
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

fn today() -> NaiveDate {
    chrono::Local::now().date_naive()
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
        return Ok(today());
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
fn over_limit(customer: &Customer, balance: Decimal) -> bool {
    customer
        .credit_limit
        .map(|limit| balance > limit)
        .unwrap_or(false)
}

/// Every active/inactive customer with the derived receivable folded in.
/// `ageing_all` only returns customers with a non-zero balance, so absent
/// entries are a zero balance with an empty ageing.
async fn customer_rows(state: &AppState) -> AppResult<Vec<CustomerRow>> {
    let as_of = today();
    let ageings: HashMap<i64, (Decimal, Ageing)> = state
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
                .unwrap_or((Decimal::ZERO, Ageing::default()));
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
) -> AppResult<Html<String>> {
    let html = CustomerListPartial {
        customers,
        warning,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

async fn list_response(state: &AppState, warning: Option<String>) -> AppResult<Response> {
    let rows = customer_rows(state).await?;
    Ok(render_list(rows, warning)?.into_response())
}

// ---------------------------------------------------------------------------
// Pages + fragments
// ---------------------------------------------------------------------------

async fn customers_page(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    principal: axum::Extension<crate::security::authz::Principal>,
) -> Result<Html<String>, AppError> {
    let customers = customer_rows(&state).await?;
    let tmpl = CustomersTemplate {
        title: "Roya — Customers".to_string(),
        customers,
        statement: None,
        selected: None,
        receipts: vec![],
        debt_sales: vec![],
        method_options: vec![],
        today: String::new(),
        warning: None,
        over_limit: false,
        drawer_open: false,
        nav_key: "customers",
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(
        tmpl.render().map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

// The statement page is the customer's own account view (statement,
// documents, receipts): the module owns it, so the single gate is
// `customers.read` — see the note on `customer_statement` in `customers_api.rs`.
async fn customer_statement_page(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
) -> Result<Html<String>, AppError> {
    let customer = state.customer_service.get_customer(id).await?;
    let statement = state.sales_service.customer_statement(id, today()).await?;
    let over_limit = over_limit(&customer, statement.balance);
    let debt_sales = state.sales_service.customer_debt_sales(id).await?;
    let receipts = state.customer_receipt_service.list_receipts(id).await?;
    let method_options = state.payment_method_service.methods_with_accounts().await?;
    let tmpl = CustomersTemplate {
        title: format!("Roya — Statement: {}", customer.name),
        customers: customer_rows(&state).await?,
        statement: Some(statement),
        selected: Some(customer),
        receipts,
        debt_sales,
        method_options,
        today: today().to_string(),
        warning: None,
        over_limit,
        drawer_open: true,
        nav_key: "customers",
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(
        tmpl.render().map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

async fn web_customer_list(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
) -> AppResult<Response> {
    list_response(&state, None).await
}

async fn web_customer_detail(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    detail_html(&state, id).await
}

async fn web_customer_edit_form(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let customer = state.customer_service.get_customer(id).await?;
    let html = CustomerEditFormPartial { customer }
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

/// The drawer body with fresh derived data: statement, documents, receipts
/// and the collect context. Both the detail fragment and the collect action
/// answer it, so collecting refreshes the balance in place without the client
/// rebuilding a URL.
async fn detail_html(state: &AppState, id: i64) -> AppResult<Html<String>> {
    let customer = state.customer_service.get_customer(id).await?;
    let statement = state.sales_service.customer_statement(id, today()).await?;
    let over_limit = over_limit(&customer, statement.balance);
    let debt_sales = state.sales_service.customer_debt_sales(id).await?;
    let receipts = state.customer_receipt_service.list_receipts(id).await?;
    let method_options = state.payment_method_service.methods_with_accounts().await?;
    let html = CustomerDetailPartial {
        statement: Some(statement),
        selected: Some(customer),
        debt_sales,
        receipts,
        method_options,
        today: today().to_string(),
        over_limit,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

async fn web_customer_receipts(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let receipts = state.customer_receipt_service.list_receipts(id).await?;
    let html = ReceiptListPartial { receipts }
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
    pub payment_days: String,
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
    pub payment_days: String,
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
    headers: HeaderMap,
    Form(form): Form<CreateCustomerForm>,
) -> AppResult<Response> {
    let result = state
        .customer_service
        .create_customer(NewCustomer {
            name: form.name,
            phone: clean_opt(&form.phone),
            address: clean_opt(&form.address),
            tax_id: clean_opt(&form.tax_id),
            notes: clean_opt(&form.notes),
            is_walkin: false,
            credit_limit: parse_opt_decimal(&form.credit_limit, "credit_limit")?,
            payment_days: parse_opt_i64(&form.payment_days, "payment_days")?,
        })
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
        Some(format!(
            "A customer named \"{}\" already exists: {}. Duplicate names are allowed.",
            result.customer.name,
            matches.join(", ")
        ))
    };
    if is_htmx(&headers) {
        let mut resp = list_response(&state, warning).await?;
        resp.headers_mut()
            .insert("HX-Trigger", "customer-created".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/customers").into_response())
}

async fn web_update_customer(
    State(state): State<AppState>,
    _: Require<CustomersWrite>,
    headers: HeaderMap,
    Form(form): Form<EditCustomerForm>,
) -> AppResult<Response> {
    // The form replaces the editable fields: empty optional values clear, an
    // empty name is rejected by the service.
    state
        .customer_service
        .update_customer(
            form.customer_id,
            UpdateCustomer {
                name: Some(form.name),
                phone: Some(clean_opt(&form.phone)),
                address: Some(clean_opt(&form.address)),
                tax_id: Some(clean_opt(&form.tax_id)),
                notes: Some(clean_opt(&form.notes)),
                credit_limit: Some(parse_opt_decimal(&form.credit_limit, "credit_limit")?),
                payment_days: Some(parse_opt_i64(&form.payment_days, "payment_days")?),
            },
        )
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(&state, None).await?;
        resp.headers_mut()
            .insert("HX-Trigger", "customer-changed".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/customers").into_response())
}

async fn web_activate_customer(
    State(state): State<AppState>,
    _: Require<CustomersWrite>,
    headers: HeaderMap,
    Form(form): Form<CustomerIdForm>,
) -> AppResult<Response> {
    state
        .customer_service
        .activate_customer(form.customer_id)
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(&state, None).await?;
        resp.headers_mut()
            .insert("HX-Trigger", "customer-changed".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/customers").into_response())
}

async fn web_deactivate_customer(
    State(state): State<AppState>,
    _: Require<CustomersWrite>,
    headers: HeaderMap,
    Form(form): Form<CustomerIdForm>,
) -> AppResult<Response> {
    state
        .customer_service
        .deactivate_customer(form.customer_id)
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(&state, None).await?;
        resp.headers_mut()
            .insert("HX-Trigger", "customer-changed".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/customers").into_response())
}

async fn web_delete_customer(
    State(state): State<AppState>,
    _: Require<CustomersWrite>,
    headers: HeaderMap,
    Form(form): Form<CustomerIdForm>,
) -> AppResult<Response> {
    state
        .customer_service
        .delete_customer(form.customer_id)
        .await?;
    if is_htmx(&headers) {
        let mut resp = list_response(&state, None).await?;
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
    headers: HeaderMap,
    Form(form): Form<CollectForm>,
) -> AppResult<Response> {
    let amount = parse_required_decimal(&form.amount, "amount")?;
    let date = parse_date_or_today(&form.date)?;
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
        return Ok(detail_html(&state, form.customer_id).await?.into_response());
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
        let account = state.account_service.create(audit_actor(&state).await, "Caja").await.unwrap();
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
            .find(|m| m.name == "Cash")
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
            })
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
            })
            .await
            .unwrap();
        let customer = state
            .customer_service
            .create_customer(NewCustomer {
                name: "Ana Web".into(),
                phone: None,
                address: None,
                tax_id: None,
                notes: None,
                is_walkin: false,
                credit_limit: Some(Decimal::from(40)),
                payment_days: Some(30),
            })
            .await
            .unwrap()
            .customer;
        let sale = state
            .sales_service
            .create_draft(NewSale {
                customer_id: customer.id,
                payment_type: PaymentType::Credit,
                sale_date: NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                due_date: Some(NaiveDate::from_ymd_opt(2024, 6, 15).unwrap()),
                receipt_no: None,
                notes: None,
            })
            .await
            .unwrap();
        state
            .sales_service
            .add_line(sale.id, product.id, Decimal::from(3), None)
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
            assert!(html.contains(expected), "page must show {expected}: {html:.600}");
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
        let (status, html) = get_html(app.clone(), &format!("/customers/{}", fixture.customer)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        for expected in [
            "Ana Web",
            "Ageing",
            "Receivable sales",
            "75",
            "over limit",
            "Collect",
            "Payment history",
        ] {
            assert!(html.contains(expected), "statement must show {expected}: {html:.600}");
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
            "current",
            "1-30",
            "75",
            "over limit",
            "Collect",
            "Payment history",
        ] {
            assert!(html.contains(expected), "detail must show {expected}: {html:.600}");
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
            "name=Juan+P%C3%A9rez&phone=555-1234&credit_limit=100&payment_days=30",
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
            &format!("customer_id={row}&name=Juan+P.&phone=&credit_limit=&payment_days=7"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(html.contains("Juan P."), "{html:.400}");
        let stored = state.customer_service.get_customer(row).await.unwrap();
        assert_eq!(stored.name, "Juan P.");
        assert_eq!(stored.phone, None);
        assert_eq!(stored.credit_limit, None);
        assert_eq!(stored.payment_days, Some(7));

        // An empty name is rejected and changes nothing.
        let (status, body) = post_form(
            app.clone(),
            "/web/customers/edit",
            &format!("customer_id={row}&name=&payment_days="),
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
        assert!(!state.customer_service.get_customer(row).await.unwrap().is_active);
        let (status, _) = post_form(
            app.clone(),
            "/web/customers/activate",
            &format!("customer_id={row}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(state.customer_service.get_customer(row).await.unwrap().is_active);

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
        let (status, html) = get_html(app.clone(), &format!("/customers/{}", fixture.customer)).await;
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
            assert!(form.contains(field), "collect form must carry {field}: {form:.600}");
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
        assert!(html.contains("30"), "the refreshed receipts must show it: {html:.600}");

        // The statement fragment now mixes the sale debit with the payment credit.
        let (status, html) = get_html(
            app.clone(),
            &format!("/web/customers/detail/{}", fixture.customer),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Payment"), "{html:.600}");

        // The page reads the derived 45 balance.
        let (status, html) = get_html(app.clone(), &format!("/customers/{}", fixture.customer)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("45"), "derived balance after collecting: {html:.600}");

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
        let (status, html) = get_html(
            app,
            &format!("/web/customers/detail/{}", fixture.customer),
        )
        .await;
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
            "name=\"payment_days\"",
        ] {
            assert!(html.contains(expected), "edit form must show {expected}: {html:.600}");
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
        let probe = test_support::seed_session_with_permissions(
            &state.pool,
            &["customers.read"],
        )
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
            body.contains("Acción no permitida"),
            "the refusal must speak Spanish: {body:.300}"
        );
        assert!(
            body.contains("customers.write"),
            "the refusal must name customers.write: {body:.300}"
        );
        let customers_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM customers")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(customers_after, customers_before, "a refused delete must write nothing");

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
        assert_eq!(receipts_after, receipts_before, "a refused collect must write nothing");
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
        assert!(body.contains("Beto Holder"), "the list must include the new customer");

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
                html.contains("Acción no permitida") && html.contains("customers.read"),
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
