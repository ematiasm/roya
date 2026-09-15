use askama::Template;
use axum::{
    extract::{Form, Query, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect},
    routing::{get, post},
    Router,
};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::str::FromStr;

use crate::error::{AppError, AppResult};
use crate::models::{MovementReason, MovementType, NewMovement, NewProduct, ProductKind, ProductStock};
use crate::repositories::{
    BarcodeRepository, CategoryRepository, ProductRepository, StockMovementRepository,
};
use crate::routes::AppState;

// ---------------------------------------------------------------------------
// Askama templates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "products.html")]
struct ProductsTemplate {
    products: Vec<ProductStock>,
    categories: Vec<crate::models::Category>,
    low_stock: Vec<ProductStock>,
    allow_negative_stock: bool,
    today: String,
}

#[derive(Template)]
#[template(path = "partials/product_list.html")]
struct ProductListPartial {
    products: Vec<ProductStock>,
}

#[derive(Template)]
#[template(path = "partials/stock_list.html")]
struct StockListPartial {
    items: Vec<ProductStock>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .map(|v| v == "true")
        .unwrap_or(false)
}

async fn all_product_stocks(state: &AppState) -> AppResult<Vec<ProductStock>> {
    let products = state.inventory_service.products.list().await?;
    let mut out = Vec::with_capacity(products.len());
    for p in products {
        let stock = state.inventory_service.movements.stock_for_product(p.id).await?;
        let suggested = match (p.min_stock, p.max_stock) {
            (Some(min), Some(max)) if stock <= min => Some(max - stock),
            _ => None,
        };
        out.push(ProductStock {
            product: p,
            stock,
            suggested,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Page + fragments
// ---------------------------------------------------------------------------

async fn products_page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let products = all_product_stocks(&state).await?;
    let categories = state.inventory_service.categories.list().await?;
    let low_stock = state.inventory_service.low_stock().await?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let tmpl = ProductsTemplate {
        products,
        categories,
        low_stock,
        allow_negative_stock: state.allow_negative_stock,
        today,
    };
    Ok(Html(
        tmpl.render().map_err(|e| AppError::Internal(e.to_string()))?,
    ))
}

#[derive(Debug, Deserialize, Default)]
pub struct WebProductFilter {
    pub category_id: Option<String>,
}

async fn web_product_list(
    State(state): State<AppState>,
    Query(q): Query<WebProductFilter>,
) -> Result<Html<String>, AppError> {
    // Empty string from <select> means "all".
    let filter: Option<i64> = q.category_id.as_deref().and_then(|s| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            t.parse().ok()
        }
    });
    let stocks = all_product_stocks(&state).await?;
    let products = match filter {
        Some(cid) => stocks
            .into_iter()
            .filter(|ps| ps.product.category_id == Some(cid))
            .collect(),
        None => stocks,
    };
    let html = ProductListPartial { products }
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

async fn web_low_stock(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let items = state.inventory_service.low_stock().await?;
    let html = StockListPartial { items }
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

async fn web_negative_stock(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let items = state.inventory_service.negative_stock().await?;
    let html = StockListPartial { items }
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

async fn web_category_options(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let cats = state.inventory_service.categories.list().await?;
    let mut html = String::from("<option value=\"\">All categories</option>");
    for c in cats {
        html.push_str(&format!(
            "<option value=\"{}\">{}</option>",
            c.id,
            html_escape(&c.name)
        ));
    }
    Ok(Html(html))
}

async fn web_product_options(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let products = state.inventory_service.products.list().await?;
    let mut html = String::new();
    for p in products {
        html.push_str(&format!(
            "<option value=\"{}\">{} — {}</option>",
            p.id,
            html_escape(&p.sku),
            html_escape(&p.name)
        ));
    }
    Ok(Html(html))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// Forms (HTMX, mirror dashboard patterns)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateCategoryForm {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub parent_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateProductForm {
    #[serde(default)]
    pub sku: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub category_id: String,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub sale_price: String,
    #[serde(default)]
    pub cost_price: String,
    #[serde(default)]
    pub track_stock: Option<String>,
    #[serde(default)]
    pub min_stock: String,
    #[serde(default)]
    pub max_stock: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateMovementForm {
    pub product_id: i64,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub qty: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub reference: String,
    #[serde(default)]
    pub date: String,
}

fn parse_opt_decimal(s: &str) -> AppResult<Option<Decimal>> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    Decimal::from_str(t)
        .map(Some)
        .map_err(|_| AppError::Validation(format!("invalid decimal: {s}")))
}

fn parse_opt_i64(s: &str) -> AppResult<Option<i64>> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    t.parse::<i64>()
        .map(Some)
        .map_err(|_| AppError::Validation(format!("invalid id: {s}")))
}

async fn web_create_category(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateCategoryForm>,
) -> Result<axum::response::Response, AppError> {
    let parent_id = parse_opt_i64(&form.parent_id)?;
    state
        .inventory_service
        .create_category(&form.name, parent_id)
        .await?;
    if is_htmx(&headers) {
        let mut resp = Html(String::new()).into_response();
        resp.headers_mut()
            .insert("HX-Trigger", "category-created".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/products").into_response())
}

async fn web_create_product(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateProductForm>,
) -> Result<axum::response::Response, AppError> {
    let kind: ProductKind = if form.kind.trim().is_empty() {
        ProductKind::Product
    } else {
        form.kind
            .parse()
            .map_err(AppError::Validation)?
    };
    let sale_price = if form.sale_price.trim().is_empty() {
        return Err(AppError::Validation("sale_price is required".into()));
    } else {
        Decimal::from_str(form.sale_price.trim())
            .map_err(|_| AppError::Validation("invalid sale_price".into()))?
    };
    let cost_price = if form.cost_price.trim().is_empty() {
        Decimal::ZERO
    } else {
        Decimal::from_str(form.cost_price.trim())
            .map_err(|_| AppError::Validation("invalid cost_price".into()))?
    };
    // Checkbox: present means checked (value "1"/"on"/"true"); absent means false.
    // A hidden default of checked in the template sends Some("1").
    let track_stock = match form.track_stock.as_deref() {
        None => false,
        Some(v) => v == "1" || v.eq_ignore_ascii_case("on") || v.eq_ignore_ascii_case("true"),
    };
    let input = NewProduct {
        sku: form.sku,
        name: form.name,
        kind,
        category_id: parse_opt_i64(&form.category_id)?,
        unit: if form.unit.trim().is_empty() {
            "un".to_string()
        } else {
            form.unit
        },
        sale_price,
        cost_price,
        track_stock,
        min_stock: parse_opt_decimal(&form.min_stock)?,
        max_stock: parse_opt_decimal(&form.max_stock)?,
        location: if form.location.trim().is_empty() {
            None
        } else {
            Some(form.location)
        },
        notes: if form.notes.trim().is_empty() {
            None
        } else {
            Some(form.notes)
        },
    };
    state.inventory_service.create_product(input).await?;
    if is_htmx(&headers) {
        let products = all_product_stocks(&state).await?;
        let html = ProductListPartial { products }
            .render()
            .map_err(|e| AppError::Internal(e.to_string()))?;
        let mut resp = Html(html).into_response();
        resp.headers_mut()
            .insert("HX-Trigger", "product-created".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/products").into_response())
}

async fn web_create_movement(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateMovementForm>,
) -> Result<axum::response::Response, AppError> {
    let movement_type: MovementType = form.kind.parse().map_err(AppError::Validation)?;
    let qty = Decimal::from_str(form.qty.trim())
        .map_err(|_| AppError::Validation("invalid qty".into()))?;
    let reason: MovementReason = if form.reason.trim().is_empty() {
        MovementReason::Initial
    } else {
        form.reason.parse().map_err(AppError::Validation)?
    };
    let date = if form.date.trim().is_empty() {
        chrono::Local::now().date_naive()
    } else {
        form.date
            .trim()
            .parse()
            .map_err(|_| AppError::Validation("invalid date (YYYY-MM-DD)".into()))?
    };
    let input = NewMovement {
        product_id: form.product_id,
        qty,
        movement_type,
        reason,
        reference: form.reference.trim().to_string(),
        date,
    };
    state.inventory_service.record_movement(input).await?;
    if is_htmx(&headers) {
        let products = all_product_stocks(&state).await?;
        let html = ProductListPartial { products }
            .render()
            .map_err(|e| AppError::Internal(e.to_string()))?;
        let mut resp = Html(html).into_response();
        resp.headers_mut()
            .insert("HX-Trigger", "movement-created".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/products").into_response())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/products", get(products_page))
        .route(
            "/web/products",
            get(web_product_list).post(web_create_product),
        )
        .route("/web/categories", post(web_create_category))
        .route("/web/category-options", get(web_category_options))
        .route("/web/product-options", get(web_product_options))
        .route("/web/stock-movements", post(web_create_movement))
        .route("/web/low-stock", get(web_low_stock))
        .route("/web/negative-stock", get(web_negative_stock))
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
    async fn web_products_page_renders() {
        let app = crate::routes::router(test_state().await);
        let (status, html) = get_html(app, "/products").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.contains("Products") || html.contains("products"),
            "page should mention products"
        );
        assert!(html.contains("Low Stock"), "page should have low-stock section");
    }

    #[tokio::test]
    async fn web_product_list_fragment_renders() {
        let app = crate::routes::router(test_state().await);
        let (status, _) = get_html(app, "/web/products").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn web_low_stock_fragment_renders() {
        let app = crate::routes::router(test_state().await);
        let (status, html) = get_html(app, "/web/low-stock").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Stock OK") || html.contains("stock-") || html.contains("low"));
    }

    #[tokio::test]
    async fn web_create_product_then_list_shows_it() {
        use axum::body::Body;
        let state = test_state().await;
        let app = crate::routes::router(state);
        let body = "sku=WEB-1&name=Web+Prod&kind=Product&unit=un&sale_price=10&cost_price=5&track_stock=1&min_stock=5&max_stock=50";
        let req = Request::builder()
            .method("POST")
            .uri("/web/products")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("HX-Request", "true")
            .body(Body::from(body))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let (status, html) = get_html(app, "/web/products").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("WEB-1"), "fragment should contain new sku: {html:.300}");
    }
}
