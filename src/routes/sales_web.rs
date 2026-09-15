// Slice D: sales web `/sales` Askama + HTMX (T7), parity with products page.
// Thin handlers over SalesService; fragments in partials/sale_*.html.
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
    allow_negative: bool,
    allow_negative_stock: bool,
    today: String,
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
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let tmpl = SalesTemplate {
        title: "All sales".to_string(),
        sales,
        debt,
        products,
        accounts,
        allow_negative: state.allow_negative,
        allow_negative_stock: state.allow_negative_stock,
        today,
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
    pub customer_name: String,
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
    pub account_id: String,
}

#[derive(Debug, Deserialize)]
pub struct RecordPaymentForm {
    pub account_id: i64,
    #[serde(default)]
    pub amount: String,
    #[serde(default)]
    pub date: String,
}

#[derive(Debug, Deserialize)]
pub struct CancelSaleForm {
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
    let _sale = state
        .sales_service
        .create_draft(crate::models::NewSale {
            customer_name: form.customer_name,
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
    let detail = state.sales_service.confirm(id, account_id).await?;
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
        .record_payment(id, form.account_id, amount, date)
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

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/sales", get(sales_page))
        .route("/web/sales", get(web_sale_list).post(web_create_sale))
        .route("/web/sales/debt", get(web_sale_debt))
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
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        AppState::new(pool, false, true)
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

    #[tokio::test]
    async fn web_create_sale_then_list_shows_it() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let body = "customer_name=Web+Client&payment_type=Cash&sale_date=2024-05-02";
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
}
