use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::models::{
    MovementReason, MovementType, NewMovement, NewProduct, NewTax, ProductKind, UpdateTax,
};
use crate::repositories::{
    BarcodeRepository, CategoryRepository, ProductRepository, StockMovementRepository,
};
use crate::routes::AppState;
use crate::security::authz::{InventoryRead, InventoryStockWrite, InventoryWrite, Require};

// S5 enforcement mapping (inventory JSON API): reads → `inventory.read`,
// product/category/barcode mutations → `inventory.write`, stock movements →
// `inventory.stock.write`. Every handler declares its own extractor: the
// module mixes read/write/stock-write on the same paths, so a router-level
// guard would over-gate the reads.

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
    /// Markup percentage. When present, the sale price is DERIVED from
    /// `cost_price` and the supplied `sale_price` is ignored; `sale_price`
    /// itself stays REQUIRED here only to keep the existing API contract
    /// unchanged.
    #[serde(default)]
    pub markup_pct: Option<Decimal>,
}

/// Patch-style PUT body for product edits, mirroring `UpdateSupplierRequest`:
/// an absent field leaves the stored value unchanged, and where clearing
/// matters the field is `Option<Option<T>>` so `null` means "clear".
/// Distinguishes an absent JSON key from an explicit `null` on a clearable field.
///
/// serde's derived `Option<Option<T>>` collapses both into the outer `None`, so
/// `null` could never mean "clear" and the patch contract would be a lie on the
/// wire. This visitor keeps the three states apart: missing key => `None` (leave
/// unchanged, supplied by `#[serde(default)]`), `null` => `Some(None)` (clear),
/// value => `Some(Some(v))` (set).
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateProductRequest {
    #[serde(default)]
    pub sku: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub kind: Option<ProductKind>,
    #[serde(default, deserialize_with = "double_option")]
    pub category_id: Option<Option<i64>>,
    #[serde(default)]
    pub unit: Option<String>,
    #[serde(default)]
    pub sale_price: Option<Decimal>,
    #[serde(default)]
    pub cost_price: Option<Decimal>,
    #[serde(default)]
    pub track_stock: Option<bool>,
    #[serde(default, deserialize_with = "double_option")]
    pub min_stock: Option<Option<Decimal>>,
    #[serde(default, deserialize_with = "double_option")]
    pub max_stock: Option<Option<Decimal>>,
    #[serde(default, deserialize_with = "double_option")]
    pub location: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    pub notes: Option<Option<String>>,
    /// Clearable like `location`/`notes`: absent leaves the stored markup
    /// (and its derived price) unchanged, `null` clears it back to a manual
    /// price keeping the last value, a value sets it and re-derives the price.
    #[serde(default, deserialize_with = "double_option")]
    pub markup_pct: Option<Option<Decimal>>,
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

#[derive(Debug, Deserialize)]
pub struct CreateTaxRequest {
    pub code: String,
    pub name: String,
    pub rate: Decimal,
    #[serde(default = "default_true")]
    pub is_active: bool,
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateTaxRequest {
    pub code: Option<String>,
    pub name: Option<String>,
    pub rate: Option<Decimal>,
    pub is_active: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct LinkProductTaxRequest {
    pub tax_id: i64,
}

fn default_true() -> bool {
    true
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
    _: Require<InventoryRead>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let cats = state.inventory_service.categories.list().await?;
    Ok(Json(serde_json::json!({ "categories": cats })))
}

async fn create_category(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Json(payload): Json<CreateCategoryRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let cat = state
        .inventory_service
        .create_category(principal.user_id, &payload.name, payload.parent_id)
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(cat))))
}

async fn get_category(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
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
    _: Require<InventoryWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateCategoryRequest>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let cat = state
        .inventory_service
        .update_category(
            principal.user_id,
            id,
            payload.name.as_deref(),
            payload.parent_id,
        )
        .await?;
    Ok(Json(serde_json::json!(cat)))
}

async fn delete_category(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
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
    _: Require<InventoryRead>,
    Query(q): Query<ProductListQuery>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let products = match q.category_id {
        Some(cid) => {
            state
                .inventory_service
                .products
                .list_by_category(cid)
                .await?
        }
        None => state.inventory_service.products.list().await?,
    };
    Ok(Json(serde_json::json!({ "products": products })))
}

async fn create_product(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
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
        // When markup_pct is present the service derives sale_price from
        // cost_price and this supplied sale_price is ignored.
        markup_pct: payload.markup_pct,
    };
    let product = state
        .inventory_service
        .create_product(principal.user_id, input)
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(product))))
}

async fn get_product(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let product = state.inventory_service.get_product(id).await?;
    Ok(Json(serde_json::json!(product)))
}

async fn update_product(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateProductRequest>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let product = state
        .inventory_service
        .update_product(
            principal.user_id,
            id,
            crate::models::UpdateProduct {
                sku: payload.sku,
                name: payload.name,
                kind: payload.kind,
                category_id: payload.category_id,
                unit: payload.unit,
                sale_price: payload.sale_price,
                cost_price: payload.cost_price,
                track_stock: payload.track_stock,
                min_stock: payload.min_stock,
                max_stock: payload.max_stock,
                location: payload.location,
                notes: payload.notes,
                // Clearable patch field: None = leave unchanged,
                // Some(None) = clear back to a manual price,
                // Some(Some(v)) = set and re-derive sale_price.
                markup_pct: payload.markup_pct,
            },
        )
        .await?;
    Ok(Json(serde_json::json!(product)))
}

async fn delete_product(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<StatusCode> {
    state.inventory_service.delete_product(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_stock(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let ps = state.inventory_service.product_stock(id).await?;
    Ok(Json(serde_json::json!(ps)))
}

async fn list_barcodes(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    state.inventory_service.get_product(id).await?;
    let codes = state.inventory_service.barcodes.list_by_product(id).await?;
    Ok(Json(serde_json::json!({ "barcodes": codes })))
}

async fn add_barcode(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    Path(id): Path<i64>,
    Json(payload): Json<AddBarcodeRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let bc = state
        .inventory_service
        .add_barcode(id, &payload.code)
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(bc))))
}

// ---------------------------------------------------------------------------
// Tax catalog + product associations
// ---------------------------------------------------------------------------

async fn list_taxes(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let taxes = state.tax_service.list_taxes().await?;
    Ok(Json(serde_json::json!({ "taxes": taxes })))
}

async fn get_tax(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    Ok(Json(serde_json::json!(
        state.tax_service.get_tax(id).await?
    )))
}

async fn create_tax(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Json(payload): Json<CreateTaxRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let tax = state
        .tax_service
        .create_tax(
            principal.user_id,
            NewTax {
                code: payload.code,
                name: payload.name,
                rate: payload.rate,
                is_active: payload.is_active,
            },
        )
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(tax))))
}

async fn update_tax(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateTaxRequest>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let tax = state
        .tax_service
        .update_tax(
            principal.user_id,
            id,
            UpdateTax {
                code: payload.code,
                name: payload.name,
                rate: payload.rate,
                is_active: payload.is_active,
            },
        )
        .await?;
    Ok(Json(serde_json::json!(tax)))
}

async fn deactivate_tax(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let tax = state
        .tax_service
        .deactivate_tax(principal.user_id, id)
        .await?;
    Ok(Json(serde_json::json!(tax)))
}

async fn list_product_taxes(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
    Path(product_id): Path<i64>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let taxes = state.tax_service.list_product_taxes(product_id).await?;
    Ok(Json(serde_json::json!({ "taxes": taxes })))
}

async fn link_product_tax(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(product_id): Path<i64>,
    Json(payload): Json<LinkProductTaxRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let link = state
        .tax_service
        .link_product_tax(principal.user_id, product_id, payload.tax_id)
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(link))))
}

async fn unlink_product_tax(
    State(state): State<AppState>,
    _: Require<InventoryWrite>,
    Path((product_id, tax_id)): Path<(i64, i64)>,
) -> crate::error::AppResult<StatusCode> {
    state
        .tax_service
        .unlink_product_tax(product_id, tax_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Stock movements + derived lists
// ---------------------------------------------------------------------------

async fn list_movements(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
    Query(q): Query<MovementListQuery>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let movements = match q.product_id {
        Some(pid) => {
            state
                .inventory_service
                .movements
                .list_by_product(pid)
                .await?
        }
        None => {
            let products = state.inventory_service.products.list().await?;
            let mut all = Vec::new();
            for p in products {
                let mut ms = state
                    .inventory_service
                    .movements
                    .list_by_product(p.id)
                    .await?;
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
    _: Require<InventoryStockWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
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
    let mov = state
        .inventory_service
        .record_movement(principal.user_id, input)
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(mov))))
}

async fn low_stock(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let items = state.inventory_service.low_stock().await?;
    Ok(Json(serde_json::json!({ "low_stock": items })))
}

async fn negative_stock(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let items = state.inventory_service.negative_stock().await?;
    Ok(Json(serde_json::json!({ "negative_stock": items })))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/categories",
            get(list_categories).post(create_category),
        )
        .route(
            "/api/categories/{id}",
            get(get_category)
                .put(update_category)
                .delete(delete_category),
        )
        .route("/api/products", get(list_products).post(create_product))
        .route(
            "/api/products/{id}",
            get(get_product).put(update_product).delete(delete_product),
        )
        .route("/api/products/{id}/stock", get(get_stock))
        .route(
            "/api/products/{id}/barcodes",
            get(list_barcodes).post(add_barcode),
        )
        .route("/api/taxes", get(list_taxes).post(create_tax))
        .route("/api/taxes/{id}", get(get_tax).put(update_tax))
        .route(
            "/api/taxes/{id}/deactivate",
            axum::routing::post(deactivate_tax),
        )
        .route(
            "/api/products/{id}/taxes",
            get(list_product_taxes).post(link_product_tax),
        )
        .route(
            "/api/products/{id}/taxes/{tax_id}",
            axum::routing::delete(unlink_product_tax),
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

    use crate::security::test_support;
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
        // S1b part 1: seed the fixed test session every request will authenticate with.
        test_support::seed_session(&pool).await.unwrap();
        AppState::new(pool, false, allow_stock)
    }

    async fn send_json(
        app: axum::Router,
        method: &str,
        uri: &str,
        cookie: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        if body.is_some() {
            builder = builder.header("content-type", "application/json");
        }
        let req = builder
            .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    // -- S5 enforcement (AC10): the permission gate on the real handlers ------

    /// A principal holding ONLY `inventory.read` can read the catalogue and is
    /// refused every mutation, in the JSON shape `/api/*` callers read. The
    /// seeded shared principal keeps working because it holds everything; the
    /// probe is a second session built for exactly this set.
    #[tokio::test]
    async fn ac10_an_inventory_read_only_principal_reads_and_is_refused_the_writes() {
        let state = test_state(true).await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["inventory.read"])
            .await
            .unwrap();
        let probe_cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        // The read the probe is allowed: the catalogue answers normally.
        let (st, _) = send_json(
            app.clone(),
            "GET",
            "/api/products",
            Some(&probe_cookie),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "inventory.read must open the reads");

        // Product mutation: the JSON refusal names the missing code.
        let (st, v) = send_json(
            app.clone(),
            "POST",
            "/api/products",
            Some(&probe_cookie),
            Some(product_body("DENIED-1")),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("inventory.write"),
            "the refusal must name inventory.write: {v}"
        );

        // Stock movement: a DIFFERENT gate, named as such.
        let (st, v) = send_json(
            app.clone(),
            "POST",
            "/api/stock-movements",
            Some(&probe_cookie),
            Some(movement_body(1, "2", "In", "Initial")),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("inventory.stock.write"),
            "the refusal must name inventory.stock.write: {v}"
        );
    }

    /// The refusal writes nothing: the probe's refused movement leaves the
    /// movements table and the derived stock exactly where they were.
    #[tokio::test]
    async fn ac10_an_inventory_refusal_writes_nothing() {
        let state = test_state(true).await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["inventory.read"])
            .await
            .unwrap();
        let probe_cookie = test_support::cookie_for(&probe);
        // The full-permission principal sets the stage.
        let app = crate::routes::router(state.clone());
        let (_, v) = send_json(
            app.clone(),
            "POST",
            "/api/products",
            Some(test_support::TEST_COOKIE),
            Some(product_body("NOWRITE")),
        )
        .await;
        let pid = v["id"].as_i64().unwrap();
        let (st, _) = send_json(
            app.clone(),
            "POST",
            "/api/stock-movements",
            Some(test_support::TEST_COOKIE),
            Some(movement_body(pid, "7", "In", "Initial")),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);

        let movements_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM stock_movements")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        let (st, before) = send_json(
            app.clone(),
            "GET",
            &format!("/api/products/{pid}/stock"),
            Some(&probe_cookie),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        let (st, v) = send_json(
            app.clone(),
            "POST",
            "/api/stock-movements",
            Some(&probe_cookie),
            Some(movement_body(pid, "3", "In", "Purchase")),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");

        let movements_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM stock_movements")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(
            movements_after, movements_before,
            "a refused request must write no movement"
        );
        let (st, after) = send_json(
            app.clone(),
            "GET",
            &format!("/api/products/{pid}/stock"),
            Some(&probe_cookie),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            after["stock"], before["stock"],
            "the derived stock must not move"
        );
    }

    /// A principal holding the permission gets its normal status on the very
    /// same endpoints: the gate is about the SET, not the route.
    #[tokio::test]
    async fn ac10_the_holding_principal_gets_the_normal_answer() {
        let state = test_state(true).await;
        let token = test_support::seed_session_with_permissions(
            &state.pool,
            &["inventory.read", "inventory.write", "inventory.stock.write"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&token);
        let app = crate::routes::router(state.clone());

        let (st, v) = send_json(
            app.clone(),
            "POST",
            "/api/products",
            Some(&cookie),
            Some(product_body("HOLDER-1")),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        let pid = v["id"].as_i64().unwrap();
        let (st, _) = send_json(
            app.clone(),
            "POST",
            "/api/stock-movements",
            Some(&cookie),
            Some(movement_body(pid, "5", "In", "Initial")),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
    }

    /// The gate runs FIRST: an anonymous request keeps the deny-by-default
    /// refusal (401 JSON), never the permission refusal — the order of the
    /// two gates is part of the contract.
    #[tokio::test]
    async fn an_anonymous_request_still_gets_the_login_gate_not_the_permission_refusal() {
        let state = test_state(true).await;
        let app = crate::routes::router(state.clone());
        let (st, v) = send_json(app.clone(), "GET", "/api/products", None, None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
        assert_eq!(v["error"].as_str(), Some("unauthorized"), "{v}");
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

    async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    async fn put_json(
        app: axum::Router,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method("PUT")
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
        let (st, _) = post_json(
            app.clone(),
            "/api/stock-movements",
            movement_body(pid, "20", "In", "Initial"),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(
            app.clone(),
            "/api/stock-movements",
            movement_body(pid, "8", "Out", "Sale"),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(
            app.clone(),
            "/api/stock-movements",
            movement_body(pid, "-2", "Adjust", "Adjust"),
        )
        .await;
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
        let (st, _) = post_json(
            app.clone(),
            "/api/stock-movements",
            movement_body(pid, "20", "In", "Initial"),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(
            app.clone(),
            "/api/stock-movements",
            movement_body(pid, "18", "Out", "Sale"),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, v) = get_json(app.clone(), "/api/low-stock").await;
        assert_eq!(st, StatusCode::OK, "low-stock: {v}");
        let items = v.get("low_stock").and_then(|x| x.as_array()).unwrap();
        let found = items.iter().find(|x| {
            x.get("product")
                .and_then(|p| p.get("id"))
                .and_then(|i| i.as_i64())
                == Some(pid)
        });
        assert!(found.is_some(), "product {pid} should be in low-stock: {v}");
        assert_eq!(
            found
                .unwrap()
                .get("suggested")
                .and_then(|x| x.as_str())
                .unwrap(),
            "48"
        );
    }

    #[tokio::test]
    async fn ac10_duplicate_barcode_conflict_and_cascade() {
        let state = test_state(true).await;
        let app = crate::routes::router(state);
        let (_, a) = post_json(app.clone(), "/api/products", product_body("BC-A")).await;
        let (_, b) = post_json(app.clone(), "/api/products", product_body("BC-B")).await;
        let aid = a.get("id").and_then(|x| x.as_i64()).unwrap();
        let bid = b.get("id").and_then(|x| x.as_i64()).unwrap();
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/products/{aid}/barcodes"),
            serde_json::json!({"code":"7790001"}),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/products/{bid}/barcodes"),
            serde_json::json!({"code":"7790001"}),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
        // Delete product without movements cascades barcodes.
        let req = Request::builder()
            .method("DELETE")
            .uri(format!("/api/products/{aid}"))
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn ac11_inventory_ops_leave_finance_untouched() {
        let state = test_state(true).await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        // Finance baseline works.
        let (st, _) = post_json(
            app.clone(),
            "/api/accounts",
            serde_json::json!({"name":"Cash"}),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, 0);
        // Inventory ops.
        let (_, v) = post_json(app.clone(), "/api/products", product_body("NOFIN")).await;
        let pid = v.get("id").and_then(|x| x.as_i64()).unwrap();
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/products/{pid}/barcodes"),
            serde_json::json!({"code":"NOFIN-BC"}),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(
            app.clone(),
            "/api/stock-movements",
            movement_body(pid, "4", "In", "Purchase"),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        // Finance untouched.
        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, 0);
        let (st, v) = get_json(app.clone(), "/api/accounts").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            v.get("accounts").and_then(|x| x.as_array()).unwrap().len(),
            1
        );
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
        let (st, _) = post_json(
            app.clone(),
            "/api/stock-movements",
            movement_body(pid, "0", "In", "Purchase"),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        // 404 unknown product movement.
        let (st, _) = post_json(
            app.clone(),
            "/api/stock-movements",
            movement_body(99999, "1", "In", "Purchase"),
        )
        .await;
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
        let (st, _) = post_json(
            app.clone(),
            "/api/stock-movements",
            movement_body(pid, "5", "In", "Purchase"),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(
            app.clone(),
            "/api/stock-movements",
            movement_body(pid, "10", "Out", "Sale"),
        )
        .await;
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
        let (st, _) = post_json(
            app.clone(),
            "/api/stock-movements",
            movement_body(pid, "5", "In", "Purchase"),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(
            app.clone(),
            "/api/stock-movements",
            movement_body(pid, "10", "Out", "Sale"),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, v) = get_json(app.clone(), "/api/negative-stock").await;
        assert_eq!(st, StatusCode::OK, "negative-stock: {v}");
        let items = v.get("negative_stock").and_then(|x| x.as_array()).unwrap();
        assert!(
            items.iter().any(|x| x
                .get("product")
                .and_then(|p| p.get("id"))
                .and_then(|i| i.as_i64())
                == Some(pid)),
            "product {pid} should be in negative-stock: {v}"
        );
    }

    // -- T1 redesign-products: PUT /api/products/{id} --------------------------

    /// A partial body changes only the named field: every other stored field
    /// survives the merge, so the patch is additive by construction.
    #[tokio::test]
    async fn put_partial_body_updates_only_that_field() {
        let state = test_state(true).await;
        let app = crate::routes::router(state);
        let (_, v) = post_json(app.clone(), "/api/products", product_body("PUT-PARTIAL")).await;
        let pid = v.get("id").and_then(|x| x.as_i64()).unwrap();
        let (st, v) = put_json(
            app.clone(),
            &format!("/api/products/{pid}"),
            serde_json::json!({ "name": "Renamed via PUT" }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "update: {v}");
        assert_eq!(
            v.get("name").and_then(|x| x.as_str()).unwrap(),
            "Renamed via PUT"
        );
        // Every other field survives.
        assert_eq!(
            v.get("sku").and_then(|x| x.as_str()).unwrap(),
            "PUT-PARTIAL"
        );
        assert_eq!(v.get("sale_price").and_then(|x| x.as_str()).unwrap(), "10");
        assert_eq!(v.get("cost_price").and_then(|x| x.as_str()).unwrap(), "5");
        assert_eq!(
            v.get("track_stock").and_then(|x| x.as_bool()).unwrap(),
            true
        );
        assert_eq!(v.get("min_stock").and_then(|x| x.as_str()).unwrap(), "5");
        assert_eq!(v.get("max_stock").and_then(|x| x.as_str()).unwrap(), "50");
    }

    /// An explicit `null` clears a clearable field while an absent key leaves
    /// the stored value unchanged. The two semantics must stay distinguishable:
    /// flattening `Option<Option<T>>` to `Option<T>` would either make an
    /// omitted key wipe stored data or make it impossible for a JSON client to
    /// blank a field without rewriting the whole record.
    #[tokio::test]
    async fn put_null_clears_location_and_notes_but_absent_keys_survive() {
        let state = test_state(true).await;
        let app = crate::routes::router(state.clone());
        let body = serde_json::json!({
            "sku": "PUT-NULL", "name": "prod PUT-NULL", "kind": "Product",
            "unit": "un", "sale_price": "10", "cost_price": "5",
            "track_stock": true, "min_stock": "5", "max_stock": "50",
            "location": "shelf A", "notes": "fragile"
        });
        let (st, v) = post_json(app.clone(), "/api/products", body).await;
        assert_eq!(st, StatusCode::CREATED, "create: {v}");
        let pid = v.get("id").and_then(|x| x.as_i64()).unwrap();

        // Absent keys leave the stored values unchanged.
        let (st, v) = put_json(
            app.clone(),
            &format!("/api/products/{pid}"),
            serde_json::json!({ "name": "Renamed, nothing cleared" }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "absent-key put: {v}");
        assert_eq!(v.get("location").and_then(|x| x.as_str()), Some("shelf A"));
        assert_eq!(v.get("notes").and_then(|x| x.as_str()), Some("fragile"));

        // Explicit `null` clears, only for the named fields; everything else
        // (including the name renamed above) survives the merge.
        let (st, v) = put_json(
            app.clone(),
            &format!("/api/products/{pid}"),
            serde_json::json!({ "location": null, "notes": null }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "null put: {v}");
        assert!(
            v.get("location")
                .map(serde_json::Value::is_null)
                .unwrap_or(false),
            "explicit null must clear location: {v}"
        );
        assert!(
            v.get("notes")
                .map(serde_json::Value::is_null)
                .unwrap_or(false),
            "explicit null must clear notes: {v}"
        );
        assert_eq!(v.get("sku").and_then(|x| x.as_str()).unwrap(), "PUT-NULL");
        assert_eq!(
            v.get("name").and_then(|x| x.as_str()).unwrap(),
            "Renamed, nothing cleared"
        );
        assert_eq!(v.get("sale_price").and_then(|x| x.as_str()).unwrap(), "10");
        assert_eq!(
            v.get("track_stock").and_then(|x| x.as_bool()).unwrap(),
            true
        );
        assert_eq!(v.get("min_stock").and_then(|x| x.as_str()).unwrap(), "5");
        assert_eq!(v.get("max_stock").and_then(|x| x.as_str()).unwrap(), "50");
    }

    #[tokio::test]
    async fn put_unknown_product_is_not_found() {
        let state = test_state(true).await;
        let app = crate::routes::router(state);
        let (st, _) = put_json(
            app,
            "/api/products/99999",
            serde_json::json!({ "name": "x" }),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    /// A body the JSON layer cannot bind (a non-numeric sale_price) is rejected
    /// by axum's extractor before the handler runs. Axum maps a serde
    /// deserialization failure to 422 Unprocessable Entity (a syntactically
    /// broken JSON body would be 400), so this status is axum's own rejection,
    /// not the AppError JSON envelope; the envelope guarantee applies to
    /// AppError responses and an extractor rejection is by design not one.
    #[tokio::test]
    async fn put_unbindable_body_is_rejected_before_the_handler() {
        let state = test_state(true).await;
        let app = crate::routes::router(state);
        let (_, v) = post_json(app.clone(), "/api/products", product_body("PUT-BAD")).await;
        let pid = v.get("id").and_then(|x| x.as_i64()).unwrap();
        let (st, _) = put_json(
            app,
            &format!("/api/products/{pid}"),
            serde_json::json!({ "sale_price": "not-a-number" }),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    }

    // -- markup-derived pricing over REST (product-markup T5) -----------------

    #[tokio::test]
    async fn create_with_markup_derives_the_sale_price_in_the_response() {
        let state = test_state(true).await;
        let app = crate::routes::router(state);
        let mut body = product_body("MK-REST-C");
        body["markup_pct"] = serde_json::json!("100");
        let (st, v) = post_json(app, "/api/products", body).await;
        assert_eq!(st, StatusCode::CREATED, "create: {v}");
        assert_eq!(v.get("sale_price").and_then(|x| x.as_str()), Some("10.00"));
        assert_eq!(v.get("markup_pct").and_then(|x| x.as_str()), Some("100"));
    }

    #[tokio::test]
    async fn update_with_markup_sets_it_and_derives_the_price() {
        let state = test_state(true).await;
        let app = crate::routes::router(state);
        let (_, v) = post_json(app.clone(), "/api/products", product_body("MK-REST-U")).await;
        let pid = v.get("id").and_then(|x| x.as_i64()).unwrap();
        let (st, v) = put_json(
            app,
            &format!("/api/products/{pid}"),
            serde_json::json!({ "markup_pct": "100" }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "update: {v}");
        assert_eq!(v.get("markup_pct").and_then(|x| x.as_str()), Some("100"));
        // cost 5 with a 100% markup derives 10.
        assert_eq!(v.get("sale_price").and_then(|x| x.as_str()), Some("10.00"));
    }

    #[tokio::test]
    async fn update_with_null_markup_clears_it_and_keeps_the_last_price() {
        let state = test_state(true).await;
        let app = crate::routes::router(state);
        let mut body = product_body("MK-REST-N");
        body["markup_pct"] = serde_json::json!("100");
        let (_, v) = post_json(app.clone(), "/api/products", body).await;
        let pid = v.get("id").and_then(|x| x.as_i64()).unwrap();
        // Explicit null is the clear state: markup goes back to "no markup,
        // manual price" while the price keeps its last derived value.
        let (st, v) = put_json(
            app,
            &format!("/api/products/{pid}"),
            serde_json::json!({ "markup_pct": null }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "clear: {v}");
        assert_eq!(v.get("markup_pct"), Some(&serde_json::Value::Null));
        assert_eq!(v.get("sale_price").and_then(|x| x.as_str()), Some("10.00"));
    }

    #[tokio::test]
    async fn update_without_the_markup_key_leaves_it_unchanged() {
        let state = test_state(true).await;
        let app = crate::routes::router(state);
        let mut body = product_body("MK-REST-K");
        body["markup_pct"] = serde_json::json!("100");
        let (_, v) = post_json(app.clone(), "/api/products", body).await;
        let pid = v.get("id").and_then(|x| x.as_i64()).unwrap();
        // An empty patch (no markup_pct key) leaves both the markup and the
        // derived price exactly as stored.
        let (st, v) = put_json(app, &format!("/api/products/{pid}"), serde_json::json!({})).await;
        assert_eq!(st, StatusCode::OK, "patch: {v}");
        assert_eq!(v.get("markup_pct").and_then(|x| x.as_str()), Some("100"));
        assert_eq!(v.get("sale_price").and_then(|x| x.as_str()), Some("10.00"));
    }
}
