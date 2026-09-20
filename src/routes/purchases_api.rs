// Slice G (T12): suppliers + product/supplier costs + purchases REST.
// Mirror sales_api/inventory_api: thin handlers over SupplierService and
// PurchasesService, no SQL here. Errors reuse AppError (400/404/409) and the
// same status codes as the existing modules (201 create, 204 delete, 200 read).
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post, put},
    Json, Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::models::{NewPurchase, NewSupplier, PaymentType, UpdatePurchaseDraft, UpdateSupplier};
use crate::repositories::{ProductRepository, ProductSupplierCostRepository};
use crate::routes::AppState;
// S7 enforcement: every handler declares the permission its action needs
// (AC10). The mapping and its judgement calls are recorded in
// openspec/changes/2026-09-18-add-identity-module/tasks.md (S7 section).
use crate::security::authz::{
    InventoryRead, PurchasesCancel, PurchasesCostsRead, PurchasesCostsWrite, PurchasesCreate,
    PurchasesRead, Require, SuppliersRead, SuppliersWrite,
};

// ---------------------------------------------------------------------------
// Request DTOs (JSON, English names)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateSupplierRequest {
    pub name: String,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateSupplierRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub phone: Option<Option<String>>,
    #[serde(default)]
    pub notes: Option<Option<String>>,
}

#[derive(Debug, Deserialize)]
pub struct RecordCostRequest {
    pub product_id: i64,
    pub supplier_id: i64,
    pub cost: Decimal,
    pub date: NaiveDate,
}

#[derive(Debug, Deserialize, Default)]
pub struct CostListQuery {
    pub product_id: Option<i64>,
    pub supplier_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct CreatePurchaseRequest {
    pub supplier_id: i64,
    pub payment_type: PaymentType,
    pub purchase_date: NaiveDate,
    #[serde(default)]
    pub due_date: Option<NaiveDate>,
    #[serde(default)]
    pub supplier_invoice_no: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdatePurchaseRequest {
    #[serde(default)]
    pub supplier_id: Option<i64>,
    #[serde(default)]
    pub payment_type: Option<PaymentType>,
    #[serde(default)]
    pub purchase_date: Option<NaiveDate>,
    #[serde(default)]
    pub due_date: Option<Option<NaiveDate>>,
    #[serde(default)]
    pub supplier_invoice_no: Option<Option<String>>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddLineRequest {
    pub product_id: i64,
    pub qty: Decimal,
    #[serde(default)]
    pub unit_cost: Option<Decimal>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateLineRequest {
    pub qty: Decimal,
    pub unit_cost: Decimal,
}

#[derive(Debug, Deserialize)]
pub struct RecordPaymentRequest {
    pub method_id: i64,
    pub amount: Decimal,
    pub date: NaiveDate,
}

/// Supplier-level payment: no receipt id, because suppliers have no grouping
/// document — the service derives the covered purchases from the supplier.
#[derive(Debug, Deserialize)]
pub struct PaySupplierRequest {
    pub supplier_id: i64,
    pub method_id: i64,
    pub amount: Decimal,
    pub date: NaiveDate,
}

#[derive(Debug, Deserialize, Default)]
pub struct ConfirmPurchaseRequest {
    #[serde(default)]
    pub method_id: Option<i64>,
}

#[derive(Debug, Deserialize, Default)]
pub struct CancelPurchaseRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers: suppliers + cost satellite
// ---------------------------------------------------------------------------

async fn list_suppliers(
    State(state): State<AppState>,
    _: Require<SuppliersRead>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let suppliers = state.supplier_service.list_suppliers().await?;
    Ok(Json(serde_json::json!({ "suppliers": suppliers })))
}

async fn create_supplier(
    State(state): State<AppState>,
    _: Require<SuppliersWrite>,
    Json(payload): Json<CreateSupplierRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let supplier = state
        .supplier_service
        .create_supplier(NewSupplier {
            name: payload.name,
            phone: payload.phone,
            notes: payload.notes,
        })
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(supplier))))
}

async fn get_supplier(
    State(state): State<AppState>,
    _: Require<SuppliersRead>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let supplier = state.supplier_service.get_supplier(id).await?;
    Ok(Json(serde_json::json!(supplier)))
}

async fn update_supplier(
    State(state): State<AppState>,
    _: Require<SuppliersWrite>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateSupplierRequest>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let supplier = state
        .supplier_service
        .update_supplier(
            id,
            UpdateSupplier {
                name: payload.name,
                phone: payload.phone,
                notes: payload.notes,
            },
        )
        .await?;
    Ok(Json(serde_json::json!(supplier)))
}

async fn activate_supplier(
    State(state): State<AppState>,
    _: Require<SuppliersWrite>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let supplier = state.supplier_service.set_active(id, true).await?;
    Ok(Json(serde_json::json!(supplier)))
}

async fn deactivate_supplier(
    State(state): State<AppState>,
    _: Require<SuppliersWrite>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let supplier = state.supplier_service.set_active(id, false).await?;
    Ok(Json(serde_json::json!(supplier)))
}

async fn delete_supplier(
    State(state): State<AppState>,
    _: Require<SuppliersWrite>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<StatusCode> {
    state.supplier_service.delete_supplier(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_costs(
    State(state): State<AppState>,
    _: Require<PurchasesCostsRead>,
    Query(q): Query<CostListQuery>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let costs = if let Some(product_id) = q.product_id {
        state
            .supplier_service
            .list_costs_for_product(product_id)
            .await?
    } else if let Some(supplier_id) = q.supplier_id {
        state
            .supplier_service
            .costs
            .list_by_supplier(supplier_id)
            .await?
    } else {
        // No filter: aggregate every product's satellite rows, like
        // `/api/stock-movements` does for movements.
        let products = state.inventory_service.products.list().await?;
        let mut all = Vec::new();
        for product in products {
            all.extend(
                state
                    .supplier_service
                    .list_costs_for_product(product.id)
                    .await?,
            );
        }
        all
    };
    Ok(Json(serde_json::json!({ "costs": costs })))
}

async fn record_cost(
    State(state): State<AppState>,
    _: Require<PurchasesCostsWrite>,
    Json(payload): Json<RecordCostRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let cost = state
        .supplier_service
        .record_cost(payload.product_id, payload.supplier_id, payload.cost, payload.date)
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(cost))))
}

// ---------------------------------------------------------------------------
// Handlers: purchases
// ---------------------------------------------------------------------------

async fn list_purchases(
    State(state): State<AppState>,
    _: Require<PurchasesRead>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let purchases = state.purchases_service.list_details().await?;
    Ok(Json(serde_json::json!({ "purchases": purchases })))
}

async fn purchase_suggestions(
    State(state): State<AppState>,
    _: Require<InventoryRead>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let suggestions = state.purchases_service.suggestions().await?;
    Ok(Json(serde_json::json!({
        "suggestions": suggestions.suggestions,
        "without_supplier": suggestions.without_supplier,
    })))
}

async fn create_purchase(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    Json(payload): Json<CreatePurchaseRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let purchase = state
        .purchases_service
        .create_draft(NewPurchase {
            supplier_id: payload.supplier_id,
            payment_type: payload.payment_type,
            purchase_date: payload.purchase_date,
            due_date: payload.due_date,
            supplier_invoice_no: payload.supplier_invoice_no,
            notes: payload.notes,
        })
        .await?;
    let detail = state.purchases_service.get_detail(purchase.id).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(detail))))
}

async fn get_purchase(
    State(state): State<AppState>,
    _: Require<PurchasesRead>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let detail = state.purchases_service.get_detail(id).await?;
    Ok(Json(serde_json::json!(detail)))
}

async fn update_purchase(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdatePurchaseRequest>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    state
        .purchases_service
        .update_draft(
            id,
            UpdatePurchaseDraft {
                supplier_id: payload.supplier_id,
                payment_type: payload.payment_type,
                purchase_date: payload.purchase_date,
                due_date: payload.due_date,
                supplier_invoice_no: payload.supplier_invoice_no,
                notes: payload.notes,
            },
        )
        .await?;
    let detail = state.purchases_service.get_detail(id).await?;
    Ok(Json(serde_json::json!(detail)))
}

async fn add_line(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    Path(id): Path<i64>,
    Json(payload): Json<AddLineRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let line = state
        .purchases_service
        .add_line(id, payload.product_id, payload.qty, payload.unit_cost)
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(line))))
}

async fn update_line(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    Path(line_id): Path<i64>,
    Json(payload): Json<UpdateLineRequest>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let line = state
        .purchases_service
        .update_line(line_id, payload.qty, payload.unit_cost)
        .await?;
    Ok(Json(serde_json::json!(line)))
}

async fn remove_line(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    Path(line_id): Path<i64>,
) -> crate::error::AppResult<StatusCode> {
    state.purchases_service.remove_line(line_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn record_payment(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
    Json(payload): Json<RecordPaymentRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let payment = state
        .purchases_service
        .record_payment(
            principal.user_id,
            id,
            payload.method_id,
            payload.amount,
            payload.date,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(payment))))
}

/// Pay a supplier across their Confirmed purchases, oldest debt first. There is
/// no grouping receipt document here (suppliers have none): the response is the
/// payments the handover produced, one per covered purchase. The amount is
/// validated against the supplier's outstanding debt (400 naming both figures)
/// and the account is derived from the method (400 when unassigned/inactive),
/// both before any write.
async fn pay_supplier(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Json(payload): Json<PaySupplierRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let payments = state
        .purchases_service
        .pay_supplier(
            principal.user_id,
            payload.supplier_id,
            payload.method_id,
            payload.amount,
            payload.date,
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "payments": payments })),
    ))
}

async fn confirm_purchase(
    State(state): State<AppState>,
    _: Require<PurchasesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
    Json(payload): Json<ConfirmPurchaseRequest>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let detail = state.purchases_service.confirm(principal.user_id, id, payload.method_id).await?;
    Ok(Json(serde_json::json!(detail)))
}

async fn cancel_purchase(
    State(state): State<AppState>,
    _: Require<PurchasesCancel>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
    Json(payload): Json<CancelPurchaseRequest>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let detail = state.purchases_service.cancel(principal.user_id, id, payload.reason).await?;
    Ok(Json(serde_json::json!(detail)))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/suppliers", get(list_suppliers).post(create_supplier))
        .route(
            "/api/suppliers/{id}",
            get(get_supplier).put(update_supplier).delete(delete_supplier),
        )
        .route("/api/suppliers/{id}/activate", post(activate_supplier))
        .route("/api/suppliers/{id}/deactivate", post(deactivate_supplier))
        .route("/api/product-supplier-costs", get(list_costs).post(record_cost))
        .route("/api/purchases", get(list_purchases).post(create_purchase))
        .route("/api/purchases/suggestions", get(purchase_suggestions))
        .route("/api/purchases/{id}", get(get_purchase).put(update_purchase))
        .route("/api/purchases/{id}/lines", post(add_line))
        .route(
            "/api/purchases/lines/{line_id}",
            put(update_line).delete(remove_line),
        )
        .route("/api/purchases/{id}/payments", post(record_payment))
        .route("/api/supplier-payments", post(pay_supplier))
        .route("/api/purchases/{id}/confirm", post(confirm_purchase))
        .route("/api/purchases/{id}/cancel", post(cancel_purchase))
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
    use crate::security::test_support;

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
        // S1b part 1: seed the fixed test session every request will authenticate with.
        test_support::seed_session(&pool).await.unwrap();
        AppState::new(pool, false, true)
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

    async fn delete_req(app: axum::Router, uri: &str) -> StatusCode {
        let req = Request::builder()
            .method("DELETE")
            .uri(uri)
            .header("cookie", test_support::TEST_COOKIE)
            .body(Body::empty())
            .unwrap();
        app.oneshot(req).await.unwrap().status()
    }

    async fn seed_product(app: &axum::Router, sku: &str, kind: &str) -> i64 {
        let body = if kind == "Service" {
            serde_json::json!({
                "sku": sku, "name": format!("prod {sku}"), "kind": kind,
                "unit": "hr", "sale_price": "10", "cost_price": "0",
                "track_stock": false
            })
        } else {
            serde_json::json!({
                "sku": sku, "name": format!("prod {sku}"), "kind": kind,
                "unit": "un", "sale_price": "10", "cost_price": "5",
                "track_stock": true, "min_stock": "5", "max_stock": "50"
            })
        };
        let (st, v) = post_json(app.clone(), "/api/products", body).await;
        assert_eq!(st, StatusCode::CREATED, "seed product {sku}: {v}");
        v.get("id").and_then(|x| x.as_i64()).unwrap()
    }

    async fn seed_stock(app: &axum::Router, pid: i64, qty: &str) {
        let (st, _) = post_json(
            app.clone(),
            "/api/stock-movements",
            serde_json::json!({
                "product_id": pid, "qty": qty, "type": "In",
                "reason": "Initial", "date": "2024-05-01"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
    }

    async fn seed_supplier(app: &axum::Router, name: &str) -> i64 {
        let (st, v) = post_json(
            app.clone(),
            "/api/suppliers",
            serde_json::json!({ "name": name }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed supplier {name}: {v}");
        v.get("id").and_then(|x| x.as_i64()).unwrap()
    }

    async fn seed_account(app: &axum::Router, name: &str) -> i64 {
        let (st, v) = post_json(
            app.clone(),
            "/api/accounts",
            serde_json::json!({ "name": name }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        v.get("id").and_then(|x| x.as_i64()).unwrap()
    }

    /// A Cash purchase leaves the account, so with overdraft blocked the account
    /// needs funds before confirming. Route-level tests seed them through the
    /// public transactions endpoint.
    async fn fund_account(app: &axum::Router, account_id: i64, amount: &str) {
        let (st, v) = post_json(
            app.clone(),
            "/api/transactions",
            serde_json::json!({
                "account_id": account_id, "type": "Income", "amount": amount,
                "description": "seed", "date": "2024-05-01"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "fund account: {v}");
    }

    async fn cash_method_id(pool: &sqlx::SqlitePool) -> i64 {
        let row: (i64,) = sqlx::query_as("SELECT id FROM payment_methods WHERE name = 'Cash'")
            .fetch_one(pool)
            .await
            .unwrap();
        row.0
    }

    async fn allow_cash(pool: &sqlx::SqlitePool, account_id: i64) -> i64 {
        let mid = cash_method_id(pool).await;
        // Ownership, not an allowlist: assign the unassigned Cash, or duplicate
        // the name when it is already owned elsewhere in this pool.
        let assigned = sqlx::query("UPDATE payment_methods SET account_id = ? WHERE id = ? AND account_id IS NULL")
            .bind(account_id)
            .bind(mid)
            .execute(pool)
            .await
            .unwrap()
            .rows_affected();
        if assigned == 1 {
            return mid;
        }
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO payment_methods (name, account_id, is_active, created_by) \
             SELECT name, ?, is_active, ? FROM payment_methods WHERE id = ? RETURNING id",
        )
        .bind(account_id)
        .bind(test_support::audit_actor_id(pool).await.unwrap())
        .bind(mid)
        .fetch_one(pool)
        .await
        .unwrap();
        row.0
    }

    fn draft_body(supplier_id: i64, payment_type: &str) -> serde_json::Value {
        if payment_type == "Credit" {
            serde_json::json!({
                "supplier_id": supplier_id, "payment_type": payment_type,
                "purchase_date": "2024-05-02", "due_date": "2024-06-01"
            })
        } else {
            serde_json::json!({
                "supplier_id": supplier_id, "payment_type": payment_type,
                "purchase_date": "2024-05-02"
            })
        }
    }

    async fn create_draft(
        app: &axum::Router,
        supplier_id: i64,
        payment_type: &str,
    ) -> i64 {
        let (st, v) = post_json(
            app.clone(),
            "/api/purchases",
            draft_body(supplier_id, payment_type),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "create draft: {v}");
        v["purchase"]["id"].as_i64().unwrap()
    }

    async fn add_line(app: &axum::Router, purchase_id: i64, product_id: i64, qty: &str) {
        let (st, v) = post_json(
            app.clone(),
            &format!("/api/purchases/{purchase_id}/lines"),
            serde_json::json!({ "product_id": product_id, "qty": qty }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "add line: {v}");
    }

    // -- AC8: purchase_number UNIQUE, immutable, NULL only in Draft/Cancelled-from-Draft --
    #[tokio::test]
    async fn ac8_purchase_number_unique_immutable_null_only_draft_via_rest() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let pid = seed_product(&app, "AC8-P", "Product").await;
        seed_stock(&app, pid, "10").await;
        let sup = seed_supplier(&app, "AC8 SUP").await;

        // Draft has NULL purchase_number.
        let aid = create_draft(&app, sup, "Credit").await;
        let (st, v) = get_json(app.clone(), &format!("/api/purchases/{aid}")).await;
        assert_eq!(st, StatusCode::OK, "get draft: {v}");
        assert!(v["purchase"]["purchase_number"].is_null(), "got {v}");

        // Add line + confirm -> number assigned.
        add_line(&app, aid, pid, "1").await;
        let (st, v) = post_json(
            app.clone(),
            &format!("/api/purchases/{aid}/confirm"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "confirm: {v}");
        let number_a = v["purchase"]["purchase_number"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(number_a.starts_with("2024-PURCH-"), "got {number_a}");

        // Second confirmed purchase -> UNIQUE second number.
        let bid = create_draft(&app, sup, "Credit").await;
        add_line(&app, bid, pid, "1").await;
        let (st, v) = post_json(
            app.clone(),
            &format!("/api/purchases/{bid}/confirm"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "confirm b: {v}");
        let number_b = v["purchase"]["purchase_number"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(number_a, number_b);

        // Immutable: edit Confirmed -> 400, number unchanged.
        let (st, _) = put_json(
            app.clone(),
            &format!("/api/purchases/{aid}"),
            serde_json::json!({ "notes": "otro" }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, v) = get_json(app.clone(), &format!("/api/purchases/{aid}")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            v["purchase"]["purchase_number"].as_str().unwrap(),
            number_a
        );

        // Draft -> Cancelled keeps NULL number (no-op).
        let cid = create_draft(&app, sup, "Cash").await;
        let (st, v) = post_json(
            app.clone(),
            &format!("/api/purchases/{cid}/cancel"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "cancel draft: {v}");
        assert!(v["purchase"]["purchase_number"].is_null(), "got {v}");
    }

    // -- AC11: stock and finance rows reference the purchase_number --
    #[tokio::test]
    async fn ac11_stock_and_finance_rows_reference_purchase_number_via_rest() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let pid = seed_product(&app, "AC11-P", "Product").await;
        seed_stock(&app, pid, "10").await;
        let sup = seed_supplier(&app, "AC11 SUP").await;
        let acc = seed_account(&app, "caja11").await;
        fund_account(&app, acc, "1000").await;
        let cash = allow_cash(&pool, acc).await;

        // Credit confirm receives stock and posts no Expense.
        let id = create_draft(&app, sup, "Credit").await;
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/purchases/{id}/lines"),
            serde_json::json!({ "product_id": pid, "qty": "2", "unit_cost": "6" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, v) = post_json(
            app.clone(),
            &format!("/api/purchases/{id}/confirm"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "confirm: {v}");
        let number = v["purchase"]["purchase_number"]
            .as_str()
            .unwrap()
            .to_string();

        // Stock In reason Purchase references the number.
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT type, reason, reference FROM stock_movements \
             WHERE product_id = ? AND type = 'In' AND reason = 'Purchase'",
        )
        .bind(pid)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "one Purchase In: {rows:?}");
        assert_eq!(rows[0].2, number);

        // Each Credit payment posts 1 Expense with reference = number.
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/purchases/{id}/payments"),
            serde_json::json!({
                "method_id": cash, "amount": "5", "date": "2024-05-10"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT kind, amount, description FROM transactions WHERE account_id = ?",
        )
        .bind(acc)
        .fetch_all(&pool)
        .await
        .unwrap();
        let paid: Vec<_> = rows.iter().filter(|r| r.2 == number).collect();
        assert_eq!(paid.len(), 1, "payment Expense references number: {rows:?}");
        assert_eq!(paid[0].0, "Expense");
        assert_eq!(paid[0].1, "5");

        // Cancel Confirmed -> stock Out Purchase-return + Income refund, same reference.
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/purchases/{id}/cancel"),
            serde_json::json!({ "reason": "wrong order" }),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT type, reason, reference FROM stock_movements \
             WHERE product_id = ? AND type = 'Out'",
        )
        .bind(pid)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "one Purchase-return Out: {rows:?}");
        assert_eq!(rows[0].1, "Purchase-return");
        assert_eq!(rows[0].2, number);
        let refunds: Vec<(String, String)> = sqlx::query_as(
            "SELECT kind, description FROM transactions WHERE description = ? AND kind = 'Income'",
        )
        .bind(&number)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(refunds.len(), 1, "one Income refund: {refunds:?}");
    }

    // -- AC12: suggestion builder over REST --
    #[tokio::test]
    async fn ac12_suggestions_choose_supplier_and_split_without_supplier_via_rest() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let costed = seed_product(&app, "SUG-COSTED", "Product").await;
        seed_stock(&app, costed, "3").await; // suggested 47
        let unsourced = seed_product(&app, "SUG-NONE", "Product").await; // stock 0, suggested 50
        let sup = seed_supplier(&app, "SUG SUP").await;
        let (st, v) = post_json(
            app.clone(),
            "/api/product-supplier-costs",
            serde_json::json!({
                "product_id": costed, "supplier_id": sup, "cost": "7.50", "date": "2024-05-01"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "record cost: {v}");

        let (st, v) = get_json(app.clone(), "/api/purchases/suggestions").await;
        assert_eq!(st, StatusCode::OK, "suggestions: {v}");
        let items = v["suggestions"].as_array().unwrap();
        assert_eq!(items.len(), 1, "one costed suggestion: {v}");
        assert_eq!(items[0]["product"]["id"].as_i64().unwrap(), costed);
        assert_eq!(items[0]["suggested_qty"].as_str().unwrap(), "47");
        assert_eq!(items[0]["supplier_id"].as_i64().unwrap(), sup);
        assert_eq!(items[0]["supplier_name"].as_str().unwrap(), "SUG SUP");
        assert_eq!(items[0]["unit_cost"].as_str().unwrap(), "7.50");
        assert_eq!(items[0]["subtotal"].as_str().unwrap(), "352.50");

        let unsourced_items = v["without_supplier"].as_array().unwrap();
        assert_eq!(unsourced_items.len(), 1, "one without-supplier: {v}");
        assert_eq!(
            unsourced_items[0]["product"]["id"].as_i64().unwrap(),
            unsourced
        );
        assert_eq!(unsourced_items[0]["suggested_qty"].as_str().unwrap(), "50");
    }

    // -- AC14: unassigned-method rejection with no side effects --
    #[tokio::test]
    async fn ac14_unassigned_method_rejected_without_side_effects_via_rest() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let pid = seed_product(&app, "AC14-P", "Product").await;
        seed_stock(&app, pid, "10").await;
        let sup = seed_supplier(&app, "AC14 SUP").await;
        let _acc = seed_account(&app, "caja14").await;
        // Cash belongs to no account: it cannot confirm or pay.
        let cash = cash_method_id(&pool).await;

        // Cash confirm with an unassigned method -> 400, no side effects.
        let id = create_draft(&app, sup, "Cash").await;
        add_line(&app, id, pid, "1").await;
        let tx_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions")
            .fetch_one(&pool)
            .await
            .unwrap();
        let moves_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM stock_movements")
            .fetch_one(&pool)
            .await
            .unwrap();
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/purchases/{id}/confirm"),
            serde_json::json!({ "method_id": cash }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "unassigned method must be 400");
        let tx_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions")
            .fetch_one(&pool)
            .await
            .unwrap();
        let moves_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM stock_movements")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tx_before.0, tx_after.0, "no finance touch on 400");
        assert_eq!(moves_before.0, moves_after.0, "no stock touch on 400");
        let (st, v) = get_json(app.clone(), &format!("/api/purchases/{id}")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["purchase"]["status"].as_str().unwrap(), "Draft");
        assert!(v["purchase"]["purchase_number"].is_null());

        // Credit payment with an unassigned method is also 400 with no finance row.
        let cid = create_draft(&app, sup, "Credit").await;
        add_line(&app, cid, pid, "1").await;
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/purchases/{cid}/confirm"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/purchases/{cid}/payments"),
            serde_json::json!({
                "method_id": cash, "amount": "1", "date": "2024-05-10"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "unassigned payment must be 400");
        let tx: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tx.0, 0, "no finance touch for unassigned payment");
    }

    // -- Supplier CRUD + satellite cost rule over REST --
    #[tokio::test]
    async fn supplier_crud_and_cost_satellite_via_rest() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let pid = seed_product(&app, "SUP-CRUD", "Product").await;

        // Create trims values; duplicate name -> 409.
        let (st, v) = post_json(
            app.clone(),
            "/api/suppliers",
            serde_json::json!({ "name": "  Sup Crud  ", "phone": " 555 ", "notes": " nota " }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "create supplier: {v}");
        let id = v["id"].as_i64().unwrap();
        assert_eq!(v["name"].as_str().unwrap(), "Sup Crud");
        assert_eq!(v["phone"].as_str().unwrap(), "555");
        let (st, _) = post_json(
            app.clone(),
            "/api/suppliers",
            serde_json::json!({ "name": "Sup Crud" }),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);

        let (st, _) = get_json(app.clone(), &format!("/api/suppliers/{id}")).await;
        assert_eq!(st, StatusCode::OK);
        let (st, v) = put_json(
            app.clone(),
            &format!("/api/suppliers/{id}"),
            serde_json::json!({ "phone": "111" }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "update supplier: {v}");
        assert_eq!(v["phone"].as_str().unwrap(), "111");

        // Satellite: a second different cost shifts current -> previous.
        let (st, v) = post_json(
            app.clone(),
            "/api/product-supplier-costs",
            serde_json::json!({
                "product_id": pid, "supplier_id": id, "cost": "10", "date": "2024-05-01"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "first cost: {v}");
        let (st, v) = post_json(
            app.clone(),
            "/api/product-supplier-costs",
            serde_json::json!({
                "product_id": pid, "supplier_id": id, "cost": "12", "date": "2024-05-02"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "second cost: {v}");
        assert_eq!(v["current_cost"].as_str().unwrap(), "12");
        assert_eq!(v["previous_cost"].as_str().unwrap(), "10");
        let (st, v) = get_json(
            app.clone(),
            &format!("/api/product-supplier-costs?product_id={pid}"),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "list costs: {v}");
        assert_eq!(v["costs"].as_array().unwrap().len(), 1);
        // Same row via the supplier filter and via the unfiltered aggregate.
        let (st, v) = get_json(
            app.clone(),
            &format!("/api/product-supplier-costs?supplier_id={id}"),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "supplier costs: {v}");
        assert_eq!(v["costs"].as_array().unwrap().len(), 1);
        let (st, v) = get_json(app.clone(), "/api/product-supplier-costs").await;
        assert_eq!(st, StatusCode::OK, "all costs: {v}");
        assert_eq!(v["costs"].as_array().unwrap().len(), 1);

        // Deactivate / activate / RESTRICT-aware delete.
        let (st, v) = post_json(
            app.clone(),
            &format!("/api/suppliers/{id}/deactivate"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(!v["is_active"].as_bool().unwrap());
        let (st, v) = post_json(
            app.clone(),
            &format!("/api/suppliers/{id}/activate"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert!(v["is_active"].as_bool().unwrap());
        assert_eq!(
            delete_req(app.clone(), &format!("/api/suppliers/{id}")).await,
            StatusCode::BAD_REQUEST,
            "supplier with cost rows cannot be deleted"
        );
        let temp = seed_supplier(&app, "Temp Sup").await;
        assert_eq!(
            delete_req(app.clone(), &format!("/api/suppliers/{temp}")).await,
            StatusCode::NO_CONTENT
        );
    }

    // -- T5: supplier-level payment across purchases, no receipt -------------
    #[tokio::test]
    async fn rest_pay_supplier_allocates_oldest_first_without_a_receipt() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let pid = seed_product(&app, "PAY-SUP", "Product").await;
        seed_stock(&app, pid, "10").await;
        let sup = seed_supplier(&app, "PAY SUP").await;
        let acc = seed_account(&app, "caja-pay").await;
        fund_account(&app, acc, "1000").await;
        let cash = allow_cash(&pool, acc).await;

        // One Confirmed Credit purchase: 2 × 5 = 10 due.
        let id = create_draft(&app, sup, "Credit").await;
        add_line(&app, id, pid, "2").await;
        let (st, v) = post_json(
            app.clone(),
            &format!("/api/purchases/{id}/confirm"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "confirm: {v}");

        // Pay 4 of the 10 outstanding: one payment for the covered purchase.
        let (st, v) = post_json(
            app.clone(),
            "/api/supplier-payments",
            serde_json::json!({
                "supplier_id": sup, "method_id": cash, "amount": "4", "date": "2024-06-20"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "pay supplier: {v}");
        let payments = v["payments"].as_array().unwrap();
        assert_eq!(payments.len(), 1, "one payment per covered purchase: {v}");
        assert_eq!(payments[0]["purchase_id"].as_i64(), Some(id));
        assert_eq!(payments[0]["amount"].as_str().unwrap(), "4");

        let (st, v) = get_json(app.clone(), &format!("/api/purchases/{id}")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["paid"].as_str().unwrap(), "4");
        assert_eq!(v["due"].as_str().unwrap(), "6");

        // More than the outstanding debt ⇒ 400, no further payment.
        let (st, _) = post_json(
            app.clone(),
            "/api/supplier-payments",
            serde_json::json!({
                "supplier_id": sup, "method_id": cash, "amount": "1000", "date": "2024-06-20"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "overpay must be 400");
        let (st, v) = get_json(app.clone(), &format!("/api/purchases/{id}")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["due"].as_str().unwrap(), "6", "overpay left no side effect");

        // Unknown supplier ⇒ 404, like the service.
        let (st, _) = post_json(
            app.clone(),
            "/api/supplier-payments",
            serde_json::json!({
                "supplier_id": 999999, "method_id": cash, "amount": "1", "date": "2024-06-20"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    // -- S7 enforcement (AC10): the permission gates on the real handlers ------

    /// Like [`post_json`]/[`get_json`], but with an explicit cookie: `None`
    /// means the truly anonymous request (the shared TEST_COOKIE belongs to
    /// the full-permission principal), and `Some(cookie)` drives a probe
    /// principal minted by [`test_support::seed_session_with_permissions`].
    async fn send_json_as(
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

    /// One draft purchase with one line behind real stock, created by the
    /// shared full-permission principal, so a limited probe can be refused
    /// acting on it. Returns (purchase_id, line_id, supplier_id).
    async fn draft_purchase_with_line(app: &axum::Router, sku: &str) -> (i64, i64, i64) {
        let pid = seed_product(app, sku, "Product").await;
        seed_stock(app, pid, "100").await;
        let sup = seed_supplier(app, "S7 SUP").await;
        let id = create_draft(app, sup, "Credit").await;
        let (st, v) = send_json_as(
            app.clone(),
            "POST",
            &format!("/api/purchases/{id}/lines"),
            Some(test_support::TEST_COOKIE),
            Some(serde_json::json!({ "product_id": pid, "qty": "2" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed line: {v}");
        let line_id = v["id"].as_i64().unwrap();
        (id, line_id, sup)
    }

    /// The read gates are real too: a principal WITHOUT the read permissions
    /// (it holds an unrelated permission, so this is not a broken fixture) is
    /// refused every purchases/suppliers/costs read, each naming its own code.
    /// The suggestions fragment is stock-derived data: `inventory.read`.
    #[tokio::test]
    async fn the_read_gates_refuse_a_principal_without_the_read_permission() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let (purchase, _line, sup) = draft_purchase_with_line(&app, "S7-NOREAD").await;
        let probe = test_support::seed_session_with_permissions(&pool, &["customers.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);

        for (uri, code) in [
            ("/api/purchases".to_string(), "purchases.read"),
            (format!("/api/purchases/{purchase}"), "purchases.read"),
            ("/api/purchases/suggestions".to_string(), "inventory.read"),
            ("/api/suppliers".to_string(), "suppliers.read"),
            (format!("/api/suppliers/{sup}"), "suppliers.read"),
            (
                "/api/product-supplier-costs".to_string(),
                "purchases.costs.read",
            ),
        ] {
            let (st, v) = send_json_as(app.clone(), "GET", &uri, Some(&cookie), None).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{uri}: {v}");
            assert!(
                v["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(code),
                "{uri} must name {code}: {v}"
            );
        }
    }

    /// A principal holding ONLY the read permissions reads everything and is
    /// refused every mutation, each naming its own code: the draft lifecycle
    /// is `purchases.create`, cancelling `purchases.cancel`, supplier entity
    /// writes `suppliers.write`, the per-supplier cost satellite
    /// `purchases.costs.write` — and a supplier payment is a purchase-side
    /// movement, so it answers to `purchases.create`, not `suppliers.write`
    /// (the S7 judgement call, recorded in the mapping).
    #[tokio::test]
    async fn ac10_a_purchases_read_only_principal_reads_and_is_refused_the_writes() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let (purchase, line, sup) = draft_purchase_with_line(&app, "S7-RO").await;
        let probe = test_support::seed_session_with_permissions(
            &pool,
            &["purchases.read", "suppliers.read", "purchases.costs.read"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&probe);

        // The reads the probe is allowed.
        for uri in [
            "/api/purchases".to_string(),
            format!("/api/purchases/{purchase}"),
            "/api/suppliers".to_string(),
            format!("/api/suppliers/{sup}"),
            "/api/product-supplier-costs?product_id=1".to_string(),
        ] {
            let (st, _) = send_json_as(app.clone(), "GET", &uri, Some(&cookie), None).await;
            assert_eq!(st, StatusCode::OK, "{uri} must open for the read probe");
        }

        // Draft creation and header edit: purchases.create.
        for (method, uri, body) in [
            (
                "POST",
                "/api/purchases".to_string(),
                Some(serde_json::json!({
                    "supplier_id": sup, "payment_type": "Credit", "purchase_date": "2024-05-02"
                })),
            ),
            (
                "PUT",
                format!("/api/purchases/{purchase}"),
                Some(serde_json::json!({ "notes": "hacked" })),
            ),
            (
                "POST",
                format!("/api/purchases/{purchase}/lines"),
                Some(serde_json::json!({ "product_id": 1, "qty": "1" })),
            ),
            (
                "PUT",
                format!("/api/purchases/lines/{line}"),
                Some(serde_json::json!({ "qty": "9", "unit_cost": "9" })),
            ),
            ("DELETE", format!("/api/purchases/lines/{line}"), None),
            (
                "POST",
                format!("/api/purchases/{purchase}/confirm"),
                Some(serde_json::json!({})),
            ),
            (
                "POST",
                format!("/api/purchases/{purchase}/payments"),
                Some(serde_json::json!({ "method_id": 1, "amount": "5", "date": "2024-05-03" })),
            ),
            (
                "POST",
                "/api/supplier-payments".to_string(),
                Some(serde_json::json!({ "supplier_id": sup, "method_id": 1, "amount": "5", "date": "2024-05-03" })),
            ),
        ] {
            let (st, v) = send_json_as(app.clone(), method, &uri, Some(&cookie), body).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{method} {uri}: {v}");
            assert!(
                v["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("purchases.create"),
                "{method} {uri} must name purchases.create: {v}"
            );
        }

        // Cancel: its own tier.
        let (st, v) = send_json_as(
            app.clone(),
            "POST",
            &format!("/api/purchases/{purchase}/cancel"),
            Some(&cookie),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("purchases.cancel"),
            "the refusal must name purchases.cancel: {v}"
        );

        // Supplier entity writes: suppliers.write, all five.
        for (method, uri, body) in [
            (
                "POST",
                "/api/suppliers".to_string(),
                Some(serde_json::json!({ "name": "Denied Supplier" })),
            ),
            (
                "PUT",
                format!("/api/suppliers/{sup}"),
                Some(serde_json::json!({ "phone": "hacked" })),
            ),
            ("POST", format!("/api/suppliers/{sup}/activate"), None),
            ("POST", format!("/api/suppliers/{sup}/deactivate"), None),
            ("DELETE", format!("/api/suppliers/{sup}"), None),
        ] {
            let (st, v) = send_json_as(app.clone(), method, &uri, Some(&cookie), body).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{method} {uri}: {v}");
            assert!(
                v["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("suppliers.write"),
                "{method} {uri} must name suppliers.write: {v}"
            );
        }

        // The cost satellite: purchases.costs.write.
        let (st, v) = send_json_as(
            app.clone(),
            "POST",
            "/api/product-supplier-costs",
            Some(&cookie),
            Some(serde_json::json!({
                "product_id": 1, "supplier_id": sup, "cost": "10", "date": "2024-05-01"
            })),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("purchases.costs.write"),
            "the refusal must name purchases.costs.write: {v}"
        );

        // Fail-closed edge, recorded in the mapping: the suggestions fragment
        // is stock-derived, so the read-only purchases probe (without
        // inventory.read) is refused it too.
        let (st, v) = send_json_as(
            app,
            "GET",
            "/api/purchases/suggestions",
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("inventory.read"),
            "the refusal must name inventory.read: {v}"
        );
    }

    /// The refusal writes nothing: the refused draft creation leaves the
    /// purchases table where it was, the refused supplier write leaves the
    /// suppliers table where it was, the refused cost records no satellite
    /// row, and the refused payment posts no payment row (nor its Expense).
    #[tokio::test]
    async fn ac10_a_purchases_refusal_writes_nothing() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let (purchase, _line, sup) = draft_purchase_with_line(&app, "S7-NOWRITE").await;
        let acc = seed_account(&app, "cajaS7").await;
        fund_account(&app, acc, "1000").await;
        let cash = allow_cash(&pool, acc).await;
        let probe = test_support::seed_session_with_permissions(
            &pool,
            &["purchases.read", "suppliers.read", "purchases.costs.read"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&probe);

        // Refused draft creation.
        let purchases_before: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM purchases").fetch_one(&pool).await.unwrap();
        let (st, v) = send_json_as(
            app.clone(),
            "POST",
            "/api/purchases",
            Some(&cookie),
            Some(serde_json::json!({
                "supplier_id": sup, "payment_type": "Credit", "purchase_date": "2024-05-02"
            })),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        let purchases_after: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM purchases").fetch_one(&pool).await.unwrap();
        assert_eq!(purchases_after, purchases_before, "a refused create must write nothing");

        // Refused supplier create and refused cost record.
        let suppliers_before: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM suppliers").fetch_one(&pool).await.unwrap();
        let (st, _) = send_json_as(
            app.clone(),
            "POST",
            "/api/suppliers",
            Some(&cookie),
            Some(serde_json::json!({ "name": "Denied Supplier" })),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let suppliers_after: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM suppliers").fetch_one(&pool).await.unwrap();
        assert_eq!(suppliers_after, suppliers_before, "a refused supplier create must write nothing");

        let costs_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM product_supplier_costs")
            .fetch_one(&pool)
            .await
            .unwrap();
        let (st, _) = send_json_as(
            app.clone(),
            "POST",
            "/api/product-supplier-costs",
            Some(&cookie),
            Some(serde_json::json!({
                "product_id": 1, "supplier_id": sup, "cost": "10", "date": "2024-05-01"
            })),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let costs_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM product_supplier_costs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(costs_after, costs_before, "a refused cost must write nothing");

        // Confirm as the shared principal, then refuse both payment paths.
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/purchases/{purchase}/confirm"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "shared confirm must run");
        let payments_before: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM purchase_payments").fetch_one(&pool).await.unwrap();
        for uri in [
            format!("/api/purchases/{purchase}/payments"),
            "/api/supplier-payments".to_string(),
        ] {
            let (st, v) = send_json_as(
                app.clone(),
                "POST",
                &uri,
                Some(&cookie),
                Some(serde_json::json!({
                    "method_id": cash, "amount": "5", "date": "2024-05-03"
                })),
            )
            .await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{uri}: {v}");
        }
        let payments_after: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM purchase_payments").fetch_one(&pool).await.unwrap();
        assert_eq!(payments_after, payments_before, "a refused payment must write nothing");

        // A refused cancel leaves the purchase Confirmed.
        let (st, _) = send_json_as(
            app.clone(),
            "POST",
            &format!("/api/purchases/{purchase}/cancel"),
            Some(&cookie),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, v) = get_json(app.clone(), &format!("/api/purchases/{purchase}")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            v["purchase"]["status"].as_str().unwrap(),
            "Confirmed",
            "a refused cancel must not flip the status"
        );
    }

    /// A principal holding the permissions gets the normal answers: draft,
    /// line, confirm, payment, supplier-level payment, cancel, supplier CRUD
    /// and cost recording all behave as before the gates.
    #[tokio::test]
    async fn ac10_the_purchases_holding_principal_gets_the_normal_answer() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let holder = test_support::seed_session_with_permissions(
            &pool,
            &[
                "purchases.read",
                "purchases.create",
                "purchases.cancel",
                "suppliers.read",
                "suppliers.write",
                "purchases.costs.read",
                "purchases.costs.write",
                "inventory.read",
            ],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&holder);
        let (purchase, _line, sup) = draft_purchase_with_line(&app, "S7-HOLDER").await;
        let acc = seed_account(&app, "cajaS7b").await;
        fund_account(&app, acc, "1000").await;
        let cash = allow_cash(&pool, acc).await;
        let pid = seed_product(&app, "S7-HOLDER-P", "Product").await;

        // Supplier create + update: 201 / 200.
        let (st, _) = send_json_as(
            app.clone(),
            "POST",
            "/api/suppliers",
            Some(&cookie),
            Some(serde_json::json!({ "name": "Holder Supplier" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = send_json_as(
            app.clone(),
            "PUT",
            &format!("/api/suppliers/{sup}"),
            Some(&cookie),
            Some(serde_json::json!({ "phone": "111" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);

        // Cost record: 201.
        let (st, _) = send_json_as(
            app.clone(),
            "POST",
            "/api/product-supplier-costs",
            Some(&cookie),
            Some(serde_json::json!({
                "product_id": pid, "supplier_id": sup, "cost": "10", "date": "2024-05-01"
            })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "record cost");

        // Confirm: purchases.create answers its normal 200 (Credit: no cash).
        let (st, _) = send_json_as(
            app.clone(),
            "POST",
            &format!("/api/purchases/{purchase}/confirm"),
            Some(&cookie),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "confirm");

        // Payment on the purchase and the supplier-level handover: both 201.
        let (st, _) = send_json_as(
            app.clone(),
            "POST",
            &format!("/api/purchases/{purchase}/payments"),
            Some(&cookie),
            Some(serde_json::json!({ "method_id": cash, "amount": "5", "date": "2024-05-03" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "payment");
        let (st, _) = send_json_as(
            app.clone(),
            "POST",
            "/api/supplier-payments",
            Some(&cookie),
            Some(serde_json::json!({ "supplier_id": sup, "method_id": cash, "amount": "5", "date": "2024-05-03" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "pay supplier");

        // Cancel: purchases.cancel answers its normal 200.
        let (st, v) = send_json_as(
            app,
            "POST",
            &format!("/api/purchases/{purchase}/cancel"),
            Some(&cookie),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "cancel: {v}");
        assert_eq!(v["purchase"]["status"].as_str().unwrap(), "Cancelled");
    }

    /// The gate order must not change: an anonymous request gets the JSON
    /// unauthorized gate, never the permission refusal.
    #[tokio::test]
    async fn an_anonymous_request_still_gets_the_json_gate_not_the_permission_refusal() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let (st, v) = send_json_as(app, "GET", "/api/purchases", None, None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
        assert_eq!(v["error"], "unauthorized");
    }
}
