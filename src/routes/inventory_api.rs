use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json, Router,
    routing::get,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::models::{MovementReason, MovementType, NewMovement, NewProduct, ProductKind};
use crate::repositories::{
    BarcodeRepository, CategoryRepository, ProductRepository, StockMovementRepository,
};
use crate::routes::AppState;

// ---------------------------------------------------------------------------
// Request DTOs (JSON, English names)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateCategoryRequest {
    pub name: String,
    pub parent_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateCategoryRequest {
    pub name: Option<String>,
    #[serde(default)]
    pub parent_id: Option<Option<i64>>,
}

#[derive(Debug, Deserialize)]
pub struct CreateProductRequest {
    pub sku: String,
    pub name: String,
    pub kind: ProductKind,
    pub category_id: Option<i64>,
    pub unit: String,
    pub sale_price: Decimal,
    #[serde(default)]
    pub cost_price: Option<Decimal>,
    pub track_stock: bool,
    pub min_stock: Option<Decimal>,
    pub max_stock: Option<Decimal>,
    pub location: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateMovementRequest {
    pub product_id: i64,
    pub qty: Decimal,
    #[serde(rename = "type")]
    pub movement_type: MovementType,
    pub reason: MovementReason,
    #[serde(default)]
    pub reference: Option<String>,
    pub date: NaiveDate,
}

#[derive(Debug, Deserialize)]
pub struct AddBarcodeRequest {
    pub code: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct ProductListQuery {
    pub category_id: Option<i64>,
}

#[derive(Debug, Deserialize, Default)]
pub struct MovementListQuery {
    pub product_id: Option<i64>,
}

// ---------------------------------------------------------------------------
// Categories
// ---------------------------------------------------------------------------

async fn list_categories(
    State(state): State<AppState>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let cats = state.inventory_service.categories.list().await?;
    Ok(Json(serde_json::json!({ "categories": cats })))
}

async fn create_category(
    State(state): State<AppState>,
    Json(payload): Json<CreateCategoryRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let cat = state
        .inventory_service
        .create_category(&payload.name, payload.parent_id)
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(cat))))
}

async fn get_category(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let cat = state
        .inventory_service
        .categories
        .find_by_id(id)
        .await?
        .ok_or_else(|| crate::error::AppError::NotFound(format!("category {id} not found")))?;
    Ok(Json(serde_json::json!(cat)))
}

async fn update_category(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateCategoryRequest>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let cat = state
        .inventory_service
        .update_category(id, payload.name.as_deref(), payload.parent_id)
        .await?;
    Ok(Json(serde_json::json!(cat)))
}

async fn delete_category(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<StatusCode> {
    state.inventory_service.delete_category(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Products
// ---------------------------------------------------------------------------

async fn list_products(
    State(state): State<AppState>,
    Query(q): Query<ProductListQuery>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let products = match q.category_id {
        Some(cid) => state.inventory_service.products.list_by_category(cid).await?,
        None => state.inventory_service.products.list().await?,
    };
    Ok(Json(serde_json::json!({ "products": products })))
}

async fn create_product(
    State(state): State<AppState>,
    Json(payload): Json<CreateProductRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let input = NewProduct {
        sku: payload.sku,
        name: payload.name,
        kind: payload.kind,
        category_id: payload.category_id,
        unit: payload.unit,
        sale_price: payload.sale_price,
        cost_price: payload.cost_price.unwrap_or(Decimal::ZERO),
        track_stock: payload.track_stock,
        min_stock: payload.min_stock,
        max_stock: payload.max_stock,
        location: payload.location,
        notes: payload.notes,
    };
    let product = state.inventory_service.create_product(input).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(product))))
}

async fn get_product(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let product = state.inventory_service.get_product(id).await?;
    Ok(Json(serde_json::json!(product)))
}

async fn delete_product(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<StatusCode> {
    state.inventory_service.delete_product(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_stock(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let ps = state.inventory_service.product_stock(id).await?;
    Ok(Json(serde_json::json!(ps)))
}

async fn list_barcodes(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    state.inventory_service.get_product(id).await?;
    let codes = state.inventory_service.barcodes.list_by_product(id).await?;
    Ok(Json(serde_json::json!({ "barcodes": codes })))
}

async fn add_barcode(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<AddBarcodeRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let bc = state.inventory_service.add_barcode(id, &payload.code).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(bc))))
}

// ---------------------------------------------------------------------------
// Stock movements + derived lists
// ---------------------------------------------------------------------------

async fn list_movements(
    State(state): State<AppState>,
    Query(q): Query<MovementListQuery>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let movements = match q.product_id {
        Some(pid) => state.inventory_service.movements.list_by_product(pid).await?,
        None => {
            let products = state.inventory_service.products.list().await?;
            let mut all = Vec::new();
            for p in products {
                let mut ms = state.inventory_service.movements.list_by_product(p.id).await?;
                all.append(&mut ms);
            }
            all.sort_by_key(|m| (m.date, m.id));
            all
        }
    };
    Ok(Json(serde_json::json!({ "movements": movements })))
}

async fn create_movement(
    State(state): State<AppState>,
    Json(payload): Json<CreateMovementRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let input = NewMovement {
        product_id: payload.product_id,
        qty: payload.qty,
        movement_type: payload.movement_type,
        reason: payload.reason,
        reference: payload.reference.unwrap_or_default(),
        date: payload.date,
    };
    let mov = state.inventory_service.record_movement(input).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(mov))))
}

async fn low_stock(
    State(state): State<AppState>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let items = state.inventory_service.low_stock().await?;
    Ok(Json(serde_json::json!({ "low_stock": items })))
}

async fn negative_stock(
    State(state): State<AppState>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let items = state.inventory_service.negative_stock().await?;
    Ok(Json(serde_json::json!({ "negative_stock": items })))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/categories", get(list_categories).post(create_category))
        .route(
            "/api/categories/{id}",
            get(get_category).put(update_category).delete(delete_category),
        )
        .route("/api/products", get(list_products).post(create_product))
        .route("/api/products/{id}", get(get_product).delete(delete_product))
        .route("/api/products/{id}/stock", get(get_stock))
        .route(
            "/api/products/{id}/barcodes",
            get(list_barcodes).post(add_barcode),
        )
        .route(
            "/api/stock-movements",
            get(list_movements).post(create_movement),
        )
        .route("/api/low-stock", get(low_stock))
        .route("/api/negative-stock", get(negative_stock))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    use tower::ServiceExt;

    async fn test_state(allow_stock: bool) -> AppState {
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
        AppState::new(pool, false, allow_stock)
    }

    async fn post_json(app: axum::Router, uri: &str, body: serde_json::Value) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    fn product_body(sku: &str) -> serde_json::Value {
        serde_json::json!({
            "sku": sku, "name": format!("prod {sku}"), "kind": "Product",
            "unit": "un", "sale_price": "10", "cost_price": "5",
            "track_stock": true, "min_stock": "5", "max_stock": "50"
        })
    }

    fn movement_body(pid: i64, qty: &str, ty: &str, reason: &str) -> serde_json::Value {
        serde_json::json!({
            "product_id": pid, "qty": qty, "type": ty,
            "reason": reason, "date": "2024-01-15"
        })
    }

    #[tokio::test]
    async fn ac8_stock_sums_signed_movements_via_rest() {
        let state = test_state(true).await;
        let app = crate::routes::router(state);
        let (st, v) = post_json(app.clone(), "/api/products", product_body("AC8")).await;
        assert_eq!(st, StatusCode::CREATED, "create product: {v}");
        let pid = v.get("id").and_then(|x| x.as_i64()).unwrap();
        let (st, _) = post_json(app.clone(), "/api/stock-movements", movement_body(pid, "20", "In", "Initial")).await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(app.clone(), "/api/stock-movements", movement_body(pid, "8", "Out", "Sale")).await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(app.clone(), "/api/stock-movements", movement_body(pid, "-2", "Adjust", "Adjust")).await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, v) = get_json(app.clone(), &format!("/api/products/{pid}/stock")).await;
        assert_eq!(st, StatusCode::OK, "get stock: {v}");
        assert_eq!(v.get("stock").and_then(|x| x.as_str()).unwrap(), "10");
    }

    #[tokio::test]
    async fn ac9_low_stock_reports_suggested_via_rest() {
        let state = test_state(true).await;
        let app = crate::routes::router(state);
        let (st, v) = post_json(app.clone(), "/api/products", product_body("AC9")).await;
        assert_eq!(st, StatusCode::CREATED, "create: {v}");
        let pid = v.get("id").and_then(|x| x.as_i64()).unwrap();
        let (st, _) = post_json(app.clone(), "/api/stock-movements", movement_body(pid, "20", "In", "Initial")).await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(app.clone(), "/api/stock-movements", movement_body(pid, "18", "Out", "Sale")).await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, v) = get_json(app.clone(), "/api/low-stock").await;
        assert_eq!(st, StatusCode::OK, "low-stock: {v}");
        let items = v.get("low_stock").and_then(|x| x.as_array()).unwrap();
        let found = items.iter().find(|x| x.get("product").and_then(|p| p.get("id")).and_then(|i| i.as_i64()) == Some(pid));
        assert!(found.is_some(), "product {pid} should be in low-stock: {v}");
        assert_eq!(found.unwrap().get("suggested").and_then(|x| x.as_str()).unwrap(), "48");
    }

    #[tokio::test]
    async fn ac10_duplicate_barcode_conflict_and_cascade() {
        let state = test_state(true).await;
        let app = crate::routes::router(state);
        let (_, a) = post_json(app.clone(), "/api/products", product_body("BC-A")).await;
        let (_, b) = post_json(app.clone(), "/api/products", product_body("BC-B")).await;
        let aid = a.get("id").and_then(|x| x.as_i64()).unwrap();
        let bid = b.get("id").and_then(|x| x.as_i64()).unwrap();
        let (st, _) = post_json(app.clone(), &format!("/api/products/{aid}/barcodes"), serde_json::json!({"code":"7790001"})).await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(app.clone(), &format!("/api/products/{bid}/barcodes"), serde_json::json!({"code":"7790001"})).await;
        assert_eq!(st, StatusCode::CONFLICT);
        // Delete product without movements cascades barcodes.
        let req = Request::builder().method("DELETE").uri(format!("/api/products/{aid}")).body(Body::empty()).unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn ac11_inventory_ops_leave_finance_untouched() {
        let state = test_state(true).await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        // Finance baseline works.
        let (st, _) = post_json(app.clone(), "/api/accounts", serde_json::json!({"name":"Cash"})).await;
        assert_eq!(st, StatusCode::CREATED);
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions").fetch_one(&pool).await.unwrap();
        assert_eq!(row.0, 0);
        // Inventory ops.
        let (_, v) = post_json(app.clone(), "/api/products", product_body("NOFIN")).await;
        let pid = v.get("id").and_then(|x| x.as_i64()).unwrap();
        let (st, _) = post_json(app.clone(), &format!("/api/products/{pid}/barcodes"), serde_json::json!({"code":"NOFIN-BC"})).await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(app.clone(), "/api/stock-movements", movement_body(pid, "4", "In", "Purchase")).await;
        assert_eq!(st, StatusCode::CREATED);
        // Finance untouched.
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions").fetch_one(&pool).await.unwrap();
        assert_eq!(row.0, 0);
        let (st, v) = get_json(app.clone(), "/api/accounts").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v.get("accounts").and_then(|x| x.as_array()).unwrap().len(), 1);
    }

    // -- triangulate: error mapping + negative guard via REST --

    #[tokio::test]
    async fn tri_rest_error_mapping_400_404_409() {
        let state = test_state(true).await;
        let app = crate::routes::router(state);
        // 409 duplicate sku.
        let (st, _) = post_json(app.clone(), "/api/products", product_body("DUP")).await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(app.clone(), "/api/products", product_body("DUP")).await;
        assert_eq!(st, StatusCode::CONFLICT);
        // 400 bad qty.
        let (_, v) = post_json(app.clone(), "/api/products", product_body("BADQ")).await;
        let pid = v.get("id").and_then(|x| x.as_i64()).unwrap();
        let (st, _) = post_json(app.clone(), "/api/stock-movements", movement_body(pid, "0", "In", "Purchase")).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        // 404 unknown product movement.
        let (st, _) = post_json(app.clone(), "/api/stock-movements", movement_body(99999, "1", "In", "Purchase")).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        // 404 unknown product stock.
        let (st, _) = get_json(app.clone(), "/api/products/99999/stock").await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn tri_rest_strict_mode_blocks_negative_via_http() {
        let state = test_state(false).await;
        let app = crate::routes::router(state);
        let (_, v) = post_json(app.clone(), "/api/products", product_body("STRICT")).await;
        let pid = v.get("id").and_then(|x| x.as_i64()).unwrap();
        let (st, _) = post_json(app.clone(), "/api/stock-movements", movement_body(pid, "5", "In", "Purchase")).await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(app.clone(), "/api/stock-movements", movement_body(pid, "10", "Out", "Sale")).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, v) = get_json(app.clone(), &format!("/api/products/{pid}/stock")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v.get("stock").and_then(|x| x.as_str()).unwrap(), "5");
    }

    #[tokio::test]
    async fn tri_rest_permissive_negative_listed_via_http() {
        let state = test_state(true).await;
        let app = crate::routes::router(state);
        let (_, v) = post_json(app.clone(), "/api/products", product_body("NEG")).await;
        let pid = v.get("id").and_then(|x| x.as_i64()).unwrap();
        let (st, _) = post_json(app.clone(), "/api/stock-movements", movement_body(pid, "5", "In", "Purchase")).await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(app.clone(), "/api/stock-movements", movement_body(pid, "10", "Out", "Sale")).await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, v) = get_json(app.clone(), "/api/negative-stock").await;
        assert_eq!(st, StatusCode::OK, "negative-stock: {v}");
        let items = v.get("negative_stock").and_then(|x| x.as_array()).unwrap();
        assert!(
            items.iter().any(|x| x.get("product").and_then(|p| p.get("id")).and_then(|i| i.as_i64()) == Some(pid)),
            "product {pid} should be in negative-stock: {v}"
        );
    }
}
