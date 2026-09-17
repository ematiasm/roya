// Slice G (T13): suppliers web `/suppliers` Askama + HTMX. Supplier CRUD plus
// the per-product cost satellite (record cost, derived price alert, preferred
// marker). Thin handlers over SupplierService; the fragment lives in
// partials/supplier_list.html.
use askama::Template;
use axum::{
    extract::{Form, Path, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{delete, get, post},
    Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{NewSupplier, Product, ProductSupplierCost, Supplier, UpdateSupplier};
use crate::repositories::{ProductRepository, ProductSupplierCostRepository};
use crate::routes::AppState;

// ---------------------------------------------------------------------------
// Views + Askama templates
// ---------------------------------------------------------------------------

/// One satellite cost row with the product fields the page needs to render it.
#[derive(Clone)]
pub struct SupplierCostView {
    pub cost: ProductSupplierCost,
    pub product_name: String,
    pub product_sku: String,
    pub product_unit: String,
}

#[derive(Clone)]
pub struct SupplierView {
    pub supplier: Supplier,
    pub costs: Vec<SupplierCostView>,
}

#[derive(Template)]
#[template(path = "suppliers.html")]
struct SuppliersTemplate {
    suppliers: Vec<SupplierView>,
    products: Vec<Product>,
    today: String,
    nav_key: &'static str,
}

#[derive(Template)]
#[template(path = "partials/supplier_list.html")]
struct SupplierListPartial {
    suppliers: Vec<SupplierView>,
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

fn parse_date_or_today(s: &str) -> AppResult<NaiveDate> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(chrono::Local::now().date_naive());
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

async fn supplier_views(state: &AppState) -> AppResult<Vec<SupplierView>> {
    let suppliers = state.supplier_service.list_suppliers().await?;
    let products = state.inventory_service.products.list().await?;
    let mut views = Vec::with_capacity(suppliers.len());
    for supplier in suppliers {
        let costs = state
            .supplier_service
            .costs
            .list_by_supplier(supplier.id)
            .await?;
        let mut cost_views = Vec::with_capacity(costs.len());
        for cost in costs {
            let (product_name, product_sku, product_unit) = products
                .iter()
                .find(|p| p.id == cost.product_id)
                .map(|p| (p.name.clone(), p.sku.clone(), p.unit.clone()))
                .unwrap_or_else(|| (format!("product #{}", cost.product_id), String::new(), String::new()));
            cost_views.push(SupplierCostView {
                cost,
                product_name,
                product_sku,
                product_unit,
            });
        }
        views.push(SupplierView {
            supplier,
            costs: cost_views,
        });
    }
    Ok(views)
}

fn triggered(html: String, event: &str) -> Response {
    let mut resp = Html(html).into_response();
    resp.headers_mut()
        .insert("HX-Trigger", event.parse().unwrap());
    resp
}

async fn render_list(suppliers: Vec<SupplierView>) -> AppResult<Html<String>> {
    let html = SupplierListPartial { suppliers }
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

/// Supplier fragment + `HX-Trigger` refresh event, for mutating web handlers.
async fn list_response(state: &AppState, event: &str) -> AppResult<Response> {
    let suppliers = supplier_views(state).await?;
    let html = render_list(suppliers).await?;
    Ok(triggered(html.0, event))
}

// ---------------------------------------------------------------------------
// Page + fragment
// ---------------------------------------------------------------------------

async fn suppliers_page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let suppliers = supplier_views(&state).await?;
    let products = state.inventory_service.products.list().await?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let tmpl = SuppliersTemplate {
        suppliers,
        products,
        today,
        nav_key: "suppliers",
    };
    Ok(Html(
        tmpl.render().map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

async fn web_supplier_list(State(state): State<AppState>) -> AppResult<Response> {
    let suppliers = supplier_views(&state).await?;
    Ok(render_list(suppliers).await?.into_response())
}

// ---------------------------------------------------------------------------
// Forms (HTMX)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SupplierForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize)]
pub struct EditSupplierForm {
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub phone: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize)]
pub struct RecordCostForm {
    pub product_id: i64,
    pub supplier_id: i64,
    #[serde(default)]
    pub cost: String,
    #[serde(default)]
    pub date: String,
}

async fn web_create_supplier(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<SupplierForm>,
) -> AppResult<Response> {
    state
        .supplier_service
        .create_supplier(NewSupplier {
            name: form.name,
            phone: clean_opt(&form.phone),
            notes: clean_opt(&form.notes),
        })
        .await?;
    if is_htmx(&headers) {
        return list_response(&state, "supplier-created").await;
    }
    Ok(Redirect::to("/suppliers").into_response())
}

async fn web_update_supplier(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<EditSupplierForm>,
) -> AppResult<Response> {
    state
        .supplier_service
        .update_supplier(
            form.id,
            UpdateSupplier {
                name: Some(form.name),
                phone: Some(clean_opt(&form.phone)),
                notes: Some(clean_opt(&form.notes)),
            },
        )
        .await?;
    if is_htmx(&headers) {
        return list_response(&state, "supplier-changed").await;
    }
    Ok(Redirect::to("/suppliers").into_response())
}

async fn web_activate_supplier(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    state.supplier_service.set_active(id, true).await?;
    list_response(&state, "supplier-changed").await
}

async fn web_deactivate_supplier(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    state.supplier_service.set_active(id, false).await?;
    list_response(&state, "supplier-changed").await
}

async fn web_delete_supplier(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    state.supplier_service.delete_supplier(id).await?;
    list_response(&state, "supplier-deleted").await
}

async fn web_record_cost(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RecordCostForm>,
) -> AppResult<Response> {
    let cost = parse_required_decimal(&form.cost, "cost")?;
    let date = parse_date_or_today(&form.date)?;
    state
        .supplier_service
        .record_cost(form.product_id, form.supplier_id, cost, date)
        .await?;
    if is_htmx(&headers) {
        return list_response(&state, "supplier-cost-recorded").await;
    }
    Ok(Redirect::to("/suppliers").into_response())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/suppliers", get(suppliers_page))
        .route(
            "/web/suppliers",
            get(web_supplier_list).post(web_create_supplier),
        )
        .route("/web/suppliers/edit", post(web_update_supplier))
        .route("/web/suppliers/{id}", delete(web_delete_supplier))
        .route("/web/suppliers/{id}/activate", post(web_activate_supplier))
        .route(
            "/web/suppliers/{id}/deactivate",
            post(web_deactivate_supplier),
        )
        .route("/web/supplier-costs", post(web_record_cost))
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

    async fn post_form(app: axum::Router, uri: &str, body: &str) -> StatusCode {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .body(Body::from(body.to_string()))
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn web_suppliers_page_renders() {
        let app = crate::routes::router(test_state().await);
        let (status, html) = get_html(app, "/suppliers").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Suppliers"), "page should mention Suppliers");
    }

    #[tokio::test]
    async fn web_create_supplier_then_list_shows_it() {
        let app = crate::routes::router(test_state().await);
        let body = "name=WebSupplier&phone=555&notes=nota";
        assert_eq!(
            post_form(app.clone(), "/web/suppliers", body).await,
            StatusCode::OK
        );
        let (status, html) = get_html(app.clone(), "/web/suppliers").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("WebSupplier"),
            "fragment should contain the new supplier: {html:.400}"
        );
    }
}
