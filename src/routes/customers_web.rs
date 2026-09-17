// M4 customers (Slice M). Web customers under `/customers` and
// `/customers/{id}`, Askama + HTMX, thin handlers over `CustomerService`
// (entity + rules), `SalesService` (derived receivable) and
// `CustomerReceiptService` (collect). Fragments live in
// `partials/customer_list.html`, `partials/customer_statement.html` and
// `partials/receipt_list.html`.
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
    AccountWithBalance, Ageing, Customer, CustomerStatement, NewCustomer, PaymentMethod,
    ReceiptDetail, SaleDetail, UpdateCustomer,
};
use crate::routes::AppState;

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
    accounts: Vec<AccountWithBalance>,
    methods: Vec<PaymentMethod>,
    today: String,
    warning: Option<String>,
}

#[derive(Template)]
#[template(path = "partials/customer_list.html")]
struct CustomerListPartial {
    customers: Vec<CustomerRow>,
    warning: Option<String>,
}

#[derive(Template)]
#[template(path = "partials/customer_statement.html")]
struct CustomerStatementPartial {
    statement: Option<CustomerStatement>,
    selected: Option<Customer>,
    debt_sales: Vec<SaleDetail>,
}

#[derive(Template)]
#[template(path = "partials/receipt_list.html")]
struct ReceiptListPartial {
    receipts: Vec<ReceiptDetail>,
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

async fn customers_page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let customers = customer_rows(&state).await?;
    let accounts = state.account_service.list_with_balances().await?;
    let methods = state.payment_method_service.list().await?;
    let tmpl = CustomersTemplate {
        title: "Roya — Customers".to_string(),
        customers,
        statement: None,
        selected: None,
        receipts: vec![],
        debt_sales: vec![],
        accounts,
        methods,
        today: today().to_string(),
        warning: None,
    };
    Ok(Html(
        tmpl.render().map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

async fn customer_statement_page(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Html<String>, AppError> {
    let customer = state.customer_service.get_customer(id).await?;
    let statement = state.sales_service.customer_statement(id, today()).await?;
    let debt_sales = state.sales_service.customer_debt_sales(id).await?;
    let receipts = state.customer_receipt_service.list_receipts(id).await?;
    let accounts = state.account_service.list_with_balances().await?;
    let methods = state.payment_method_service.list().await?;
    let tmpl = CustomersTemplate {
        title: format!("Roya — Statement: {}", customer.name),
        customers: vec![],
        statement: Some(statement),
        selected: Some(customer),
        receipts,
        debt_sales,
        accounts,
        methods,
        today: today().to_string(),
        warning: None,
    };
    Ok(Html(
        tmpl.render().map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

async fn web_customer_list(State(state): State<AppState>) -> AppResult<Response> {
    list_response(&state, None).await
}

async fn web_customer_statement(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<Html<String>> {
    let customer = state.customer_service.get_customer(id).await?;
    let statement = state.sales_service.customer_statement(id, today()).await?;
    let debt_sales = state.sales_service.customer_debt_sales(id).await?;
    let html = CustomerStatementPartial {
        statement: Some(statement),
        selected: Some(customer),
        debt_sales,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

async fn web_customer_receipts(
    State(state): State<AppState>,
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
    pub account_id: i64,
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
    headers: HeaderMap,
    Form(form): Form<CollectForm>,
) -> AppResult<Response> {
    let amount = parse_required_decimal(&form.amount, "amount")?;
    let date = parse_date_or_today(&form.date)?;
    state
        .customer_receipt_service
        .collect(
            form.customer_id,
            form.account_id,
            form.method_id,
            amount,
            date,
            clean_opt(&form.notes),
        )
        .await?;
    if is_htmx(&headers) {
        let receipts = state
            .customer_receipt_service
            .list_receipts(form.customer_id)
            .await?;
        let html = ReceiptListPartial { receipts }
            .render()
            .map_err(|e| AppError::Internal(e.to_string()))?;
        return Ok(Html(html).into_response());
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
        .route("/web/customers/{id}/statement", get(web_customer_statement))
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
        // Enforcement off: the fixture confirms an over-limit sale and the page
        // must report the customer as over limit (the 400 path is covered by the
        // REST and smoke suites).
        AppState::new_with_credit_limit(pool, false, true, false)
    }

    async fn get_html(app: axum::Router, uri: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap();
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
        let account = state.account_service.create("Caja").await.unwrap();
        state
            .payment_method_service
            .ensure_defaults_for_account(account.id, "Caja")
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
            .create_product(NewProduct {
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
            .record_movement(NewMovement {
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
            .confirm(sale.id, None, None)
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
    async fn web_customers_page_lists_balance_ageing_and_over_limit() {
        let state = test_state().await;
        let fixture = seed_fixture(&state).await;
        let app = crate::routes::router(state);

        let (status, html) = get_html(app.clone(), "/customers").await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        for expected in ["Ana Web", "New Customer", "Edit Customer", "75", "over limit"] {
            assert!(html.contains(expected), "page must show {expected}: {html:.600}");
        }
        assert!(
            html.contains("1-30") && html.contains("31-60") && html.contains("61+"),
            "the page must show the ageing buckets: {html:.600}"
        );

        let (status, html) = get_html(app.clone(), "/web/customers").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Ana Web"), "{html:.400}");

        // The statement page keeps the customer context and renders its fragments.
        let (status, html) = get_html(app.clone(), &format!("/customers/{}", fixture.customer)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        for expected in ["Ana Web", "Ageing", "Receivable sales", "75", "Collect"] {
            assert!(html.contains(expected), "statement must show {expected}: {html:.600}");
        }
        assert!(
            html.contains(&format!("/web/customers/{}/statement", fixture.customer)),
            "statement page must refresh through its fragment endpoint"
        );

        let (status, html) = get_html(
            app.clone(),
            &format!("/web/customers/{}/statement", fixture.customer),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Ana Web"), "{html:.400}");
        assert!(html.contains("current"), "{html:.400}");

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

        // The rendered collect form carries the customer, amount, account and
        // method, and posts to the collection endpoint with the id in the body.
        let (status, html) = get_html(app.clone(), &format!("/customers/{}", fixture.customer)).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        let form_start = html
            .find("hx-post=\"/web/customer-receipts\"")
            .expect("the collect form must post to the collection endpoint");
        let form = &html[form_start..];
        for field in [
            "name=\"customer_id\"",
            "name=\"amount\"",
            "name=\"account_id\"",
            "name=\"method_id\"",
        ] {
            assert!(form.contains(field), "collect form must carry {field}: {form:.600}");
        }

        // Collect 30 of the 75 debt.
        let (status, html) = post_form(
            app.clone(),
            "/web/customer-receipts",
            &format!(
                "customer_id={}&account_id={}&method_id={}&amount=30&date=2024-06-20&notes=part",
                fixture.customer, fixture.account, fixture.cash
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(html.contains("30"), "the refreshed receipts must show it: {html:.600}");

        // The statement fragment now mixes the sale debit with the payment credit.
        let (status, html) = get_html(
            app.clone(),
            &format!("/web/customers/{}/statement", fixture.customer),
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
                "customer_id={}&account_id={}&method_id={}&amount=1000&date=2024-06-20",
                fixture.customer, fixture.account, fixture.cash
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
        for expected in [
            "/web/customers",
            "/web/customers/edit",
            "/web/customers/deactivate",
            "/web/customers/delete",
        ] {
            assert!(
                targets.iter().any(|t| t == expected),
                "customers page must post to {expected}: {targets:?}"
            );
        }
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

        let (status, html) = get_html(app, &format!("/customers/{}", fixture.customer)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("hx-post=\"/web/customer-receipts\""),
            "statement page must collect through the collection endpoint"
        );
    }
}
