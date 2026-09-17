// Slice D: sales web `/sales` Askama + HTMX (T7), parity with products page.
// Thin handlers over SalesService; fragments in partials/sale_*.html.
// Typed-id actions (add line, confirm, pay, cancel) post to collection web
// endpoints with the sale id in the form body, because HTMX cannot interpolate
// a path from an input value; the `/web/sales/{id}/...` paths stay for existing
// callers.
use askama::Template;
use axum::{
    extract::{Form, Path, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
    Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{PaymentType, SaleDetail};
use crate::repositories::ProductRepository;
use crate::routes::AppState;

// ---------------------------------------------------------------------------
// Askama templates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "sales.html")]
struct SalesTemplate {
    title: String,
    sales: Vec<SaleDetail>,
    debt: Vec<SaleDetail>,
    products: Vec<crate::models::Product>,
    accounts: Vec<crate::models::AccountWithBalance>,
    methods: Vec<crate::models::PaymentMethod>,
    customers: Vec<crate::models::Customer>,
    allow_negative: bool,
    allow_negative_stock: bool,
    today: String,
    nav_key: &'static str,
}

#[derive(Template)]
#[template(path = "partials/sale_list.html")]
struct SaleListPartial {
    title: String,
    sales: Vec<SaleDetail>,
}

#[derive(Template)]
#[template(path = "partials/sale_detail.html")]
struct SaleDetailPartial {
    detail: SaleDetail,
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

fn parse_required_decimal(s: &str, field: &str) -> AppResult<Decimal> {
    Decimal::from_str(s.trim())
        .map_err(|_| AppError::Validation(format!("invalid {field}")))
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

fn render_list(sales: Vec<SaleDetail>, title: &str) -> AppResult<Html<String>> {
    let html = SaleListPartial {
        title: title.to_string(),
        sales,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

fn render_detail(detail: SaleDetail) -> AppResult<Html<String>> {
    let html = SaleDetailPartial { detail }
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

fn changed(detail: SaleDetail) -> AppResult<axum::response::Response> {
    let html = render_detail(detail)?.0;
    let mut resp = Html(html).into_response();
    resp.headers_mut()
        .insert("HX-Trigger", "sale-changed".parse().unwrap());
    Ok(resp)
}

// ---------------------------------------------------------------------------
// Page + fragments
// ---------------------------------------------------------------------------

async fn sales_page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let sales = state.sales_service.list_details().await?;
    let debt = state.sales_service.outstanding_debt().await?;
    let products = state.inventory_service.products.list().await?;
    let accounts = state.account_service.list_with_balances().await?;
    let methods = state.payment_method_service.list().await?;
    let customers = state.customer_service.list_customers(true).await?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let tmpl = SalesTemplate {
        title: "All sales".to_string(),
        sales,
        debt,
        products,
        accounts,
        methods,
        customers,
        allow_negative: state.allow_negative,
        allow_negative_stock: state.allow_negative_stock,
        today,
        nav_key: "sales",
    };
    Ok(Html(
        tmpl.render().map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

async fn web_sale_list(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let sales = state.sales_service.list_details().await?;
    render_list(sales, "All sales")
}

async fn web_sale_debt(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let debt = state.sales_service.outstanding_debt().await?;
    render_list(debt, "Outstanding debt")
}

async fn web_sale_detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Html<String>, AppError> {
    let detail = state.sales_service.get_detail(id).await?;
    render_detail(detail)
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
    pub product_id: i64,
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
    pub account_id: String,
    #[serde(default)]
    pub method_id: String,
}

#[derive(Debug, Deserialize)]
pub struct RecordPaymentForm {
    #[serde(default)]
    pub sale_id: i64,
    pub account_id: i64,
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

async fn web_create_sale(
    State(state): State<AppState>,
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
        Some(form.due_date.trim().parse().map_err(|_| {
            AppError::Validation("invalid due_date (YYYY-MM-DD)".into())
        })?)
    };
    let customer_id = form
        .customer_id
        .ok_or_else(|| AppError::Validation("customer is required".into()))?;
    let _sale = state
        .sales_service
        .create_draft(crate::models::NewSale {
            customer_id,
            payment_type,
            sale_date: parse_date_or_today(&form.sale_date)?,
            due_date,
            receipt_no,
            notes: Some(form.notes),
        })
        .await?;
    if is_htmx(&headers) {
        let sales = state.sales_service.list_details().await?;
        let html = render_list(sales, "All sales")?.0;
        let mut resp = Html(html).into_response();
        resp.headers_mut()
            .insert("HX-Trigger", "sale-created".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/sales").into_response())
}

async fn web_add_line(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<AddLineForm>,
) -> Result<axum::response::Response, AppError> {
    let qty = parse_required_decimal(&form.qty, "qty")?;
    let unit_price = parse_opt_decimal(&form.unit_price, "unit_price")?;
    state
        .sales_service
        .add_line(id, form.product_id, qty, unit_price)
        .await?;
    if is_htmx(&headers) {
        let detail = state.sales_service.get_detail(id).await?;
        return changed(detail);
    }
    Ok(Redirect::to("/sales").into_response())
}

async fn web_update_line(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((sale_id, line_id)): Path<(i64, i64)>,
    Form(form): Form<UpdateLineForm>,
) -> Result<axum::response::Response, AppError> {
    let qty = parse_required_decimal(&form.qty, "qty")?;
    let unit_price = parse_required_decimal(&form.unit_price, "unit_price")?;
    state
        .sales_service
        .update_line(line_id, qty, unit_price)
        .await?;
    if is_htmx(&headers) {
        let detail = state.sales_service.get_detail(sale_id).await?;
        return changed(detail);
    }
    Ok(Redirect::to("/sales").into_response())
}

async fn web_remove_line(
    State(state): State<AppState>,
    Path((sale_id, line_id)): Path<(i64, i64)>,
) -> Result<axum::response::Response, AppError> {
    state.sales_service.remove_line(line_id).await?;
    let detail = state.sales_service.get_detail(sale_id).await?;
    changed(detail)
}

async fn web_confirm_sale(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<ConfirmSaleForm>,
) -> Result<axum::response::Response, AppError> {
    let account_id = parse_opt_i64(&form.account_id, "account_id")?;
    let method_id = parse_opt_i64(&form.method_id, "method_id")?;
    let detail = state
        .sales_service
        .confirm(id, account_id, method_id)
        .await?;
    if is_htmx(&headers) {
        return changed(detail);
    }
    Ok(Redirect::to("/sales").into_response())
}

async fn web_record_payment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<RecordPaymentForm>,
) -> Result<axum::response::Response, AppError> {
    let amount = parse_required_decimal(&form.amount, "amount")?;
    let date = parse_date_or_today(&form.date)?;
    state
        .sales_service
        .record_payment(id, form.account_id, form.method_id, amount, date)
        .await?;
    if is_htmx(&headers) {
        let detail = state.sales_service.get_detail(id).await?;
        return changed(detail);
    }
    Ok(Redirect::to("/sales").into_response())
}

async fn web_cancel_sale(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CancelSaleForm>,
) -> Result<axum::response::Response, AppError> {
    let reason = if form.reason.trim().is_empty() {
        None
    } else {
        Some(form.reason.trim().to_string())
    };
    let detail = state.sales_service.cancel(id, reason).await?;
    if is_htmx(&headers) {
        return changed(detail);
    }
    Ok(Redirect::to("/sales").into_response())
}

// ---------------------------------------------------------------------------
// Collection endpoints (typed id in the form body)
// ---------------------------------------------------------------------------

// HTMX posts the literal `hx-post` URL and never reads the form `action`
// property, so typed-id forms cannot interpolate a path segment. These
// adapters take the sale id from the submitted body and delegate to the
// path-based handlers above, keeping both URL shapes working (mirrors
// `purchases_web`).

async fn web_add_line_collection(
    state: State<AppState>,
    headers: HeaderMap,
    Form(form): Form<AddLineForm>,
) -> Result<axum::response::Response, AppError> {
    web_add_line(state, headers, Path(form.sale_id), Form(form)).await
}

async fn web_confirm_sale_collection(
    state: State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ConfirmSaleForm>,
) -> Result<axum::response::Response, AppError> {
    web_confirm_sale(state, headers, Path(form.sale_id), Form(form)).await
}

async fn web_record_payment_collection(
    state: State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RecordPaymentForm>,
) -> Result<axum::response::Response, AppError> {
    web_record_payment(state, headers, Path(form.sale_id), Form(form)).await
}

async fn web_cancel_sale_collection(
    state: State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CancelSaleForm>,
) -> Result<axum::response::Response, AppError> {
    web_cancel_sale(state, headers, Path(form.sale_id), Form(form)).await
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/sales", get(sales_page))
        .route("/web/sales", get(web_sale_list).post(web_create_sale))
        .route("/web/sales/debt", get(web_sale_debt))
        .route("/web/sales/lines", post(web_add_line_collection))
        .route("/web/sales/confirm", post(web_confirm_sale_collection))
        .route("/web/sales/payments", post(web_record_payment_collection))
        .route("/web/sales/cancel", post(web_cancel_sale_collection))
        .route("/web/sales/{id}", get(web_sale_detail))
        .route("/web/sales/{id}/lines", post(web_add_line))
        .route(
            "/web/sales/{sale_id}/lines/{line_id}",
            post(web_update_line).delete(web_remove_line),
        )
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

    use crate::routes::AppState;

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
        AppState::new(pool, false, true)
    }

    async fn seed_customer(state: &AppState, name: &str) -> crate::models::Customer {
        state
            .customer_service
            .create_customer(crate::models::NewCustomer {
                name: name.into(),
                phone: None,
                address: None,
                tax_id: None,
                notes: None,
                is_walkin: false,
                credit_limit: None,
                payment_days: None,
            })
            .await
            .unwrap()
            .customer
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

    /// Regression: the typed-id forms used `hx-post="/web/sales/0/..."` plus a
    /// dead `onsubmit` action rewrite. HTMX ignores the form `.action`, so every
    /// typed-id form posted to sale 0 and the server answered 404. Pin the
    /// rendered page to the collection endpoints and prove each target resolves.
    #[tokio::test]
    async fn web_sales_forms_target_registered_collection_routes() {
        let app = crate::routes::router(test_state().await);
        let (status, html) = get_html(app.clone(), "/sales").await;
        assert_eq!(status, StatusCode::OK);

        let mut targets = Vec::new();
        let mut rest = html.as_str();
        while let Some(start) = rest.find("hx-post=\"") {
            let after = &rest[start + "hx-post=\"".len()..];
            let end = after.find('"').expect("unterminated hx-post attribute");
            targets.push(after[..end].to_string());
            rest = &after[end..];
        }
        assert!(!targets.is_empty(), "page must render hx-post forms");

        for target in &targets {
            assert!(
                !target.contains("/0/"),
                "dead `/0/` placeholder target still rendered: {target}"
            );
        }
        for expected in [
            "/web/sales/lines",
            "/web/sales/confirm",
            "/web/sales/payments",
            "/web/sales/cancel",
        ] {
            assert!(
                targets.iter().any(|t| t == expected),
                "rendered page must post to {expected}: {targets:?}"
            );
        }
        assert!(
            !html.contains("this.action="),
            "dead onsubmit action rewrite still rendered"
        );

        // Any request that does not resolve to a registered route hits this
        // sentinel, so a teapot response is a routing miss (an empty-body POST
        // to a real handler fails extraction or validation instead).
        let app = app.fallback(|| async { StatusCode::IM_A_TEAPOT });
        for target in &targets {
            let req = Request::builder()
                .method("POST")
                .uri(target)
                .header("content-type", "application/x-www-form-urlencoded")
                .header("HX-Request", "true")
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_ne!(
                resp.status(),
                StatusCode::IM_A_TEAPOT,
                "{target} does not resolve to a registered route"
            );
        }
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
            .create_product(NewProduct {
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
            })
            .await
            .unwrap();
        state
            .inventory_service
            .record_movement(NewMovement {
                product_id: product.id,
                qty: Decimal::from(10),
                movement_type: MovementType::In,
                reason: MovementReason::Initial,
                reference: String::new(),
                date: chrono::NaiveDate::from_ymd_opt(2024, 5, 1).unwrap(),
            })
            .await
            .unwrap();
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
            .expect("Cash method is seeded by migrations");

        let payer = seed_customer(&state, "Web Payer").await;

        let mut sale_ids = Vec::new();
        for _ in 0..2 {
            let sale = state
                .sales_service
                .create_draft(NewSale {
                    customer_id: payer.id,
                    payment_type: PaymentType::Credit,
                    sale_date: chrono::NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                    due_date: Some(chrono::NaiveDate::from_ymd_opt(2024, 6, 2).unwrap()),
                    receipt_no: None,
                    notes: None,
                })
                .await
                .unwrap();
            state
                .sales_service
                .add_line(sale.id, product.id, Decimal::from(2), None)
                .await
                .unwrap();
            state
                .sales_service
                .confirm(sale.id, None, None)
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
            .create_product(NewProduct {
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
            })
            .await
            .unwrap();

        let typist = seed_customer(&state, "Web Typist").await;

        let mut sale_ids = Vec::new();
        for _ in 0..2 {
            let sale = state
                .sales_service
                .create_draft(NewSale {
                    customer_id: typist.id,
                    payment_type: PaymentType::Credit,
                    sale_date: chrono::NaiveDate::from_ymd_opt(2024, 5, 2).unwrap(),
                    due_date: Some(chrono::NaiveDate::from_ymd_opt(2024, 6, 2).unwrap()),
                    receipt_no: None,
                    notes: None,
                })
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
            state.sales_service.get_detail(target).await.unwrap().sale.status,
            SaleStatus::Confirmed
        );
        assert_eq!(
            state.sales_service.get_detail(other).await.unwrap().sale.status,
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
            state.sales_service.get_detail(target).await.unwrap().sale.status,
            SaleStatus::Cancelled
        );
        assert_eq!(
            state.sales_service.get_detail(other).await.unwrap().sale.status,
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

    #[tokio::test]
    async fn web_create_sale_then_list_shows_it() {
        let state = test_state().await;
        let customer = seed_customer(&state, "Web Client").await;
        let app = crate::routes::router(state);
        let body = format!(
            "customer_id={}&payment_type=Cash&sale_date=2024-05-02",
            customer.id
        );
        let req = Request::builder()
            .method("POST")
            .uri("/web/sales")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let (status, html) = get_html(app, "/web/sales").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("Web Client"),
            "fragment should contain new customer: {html:.300}"
        );
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
        let (status, body) = post_form(
            app,
            "/web/sales",
            "payment_type=Cash&sale_date=2024-05-02",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.to_lowercase().contains("customer"), "{body}");
    }
}
