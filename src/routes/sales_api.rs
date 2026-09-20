// Slice D: sales REST under `/api/sales` (T6).
// Mirror inventory_api patterns: thin handlers over SalesService, no SQL here.
// Errors reuse AppError (400 Validation / 404 NotFound / 409 Conflict).
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post, put},
    Json, Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::models::{PaymentType, UpdateSaleDraft};
use crate::routes::AppState;

// S6 enforcement (AC10): the extractor declares the permission each action
// needs. Reads are `sales.read`; the draft lifecycle (create, edit, lines,
// confirm) is `sales.create` — the catalog has no `sales.write`, so recording
// a sale IS editing the draft; cancelling is its own tier `sales.cancel`;
// recording money on a confirmed sale is the collection capability
// `customers.collect` (see the note on `record_payment`).
use crate::security::authz::{CustomersCollect, Require, SalesCancel, SalesCreate, SalesRead};

// ---------------------------------------------------------------------------
// Request DTOs (JSON, English names)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CreateSaleRequest {
    #[serde(default)]
    pub customer_id: Option<i64>,
    pub payment_type: PaymentType,
    pub sale_date: NaiveDate,
    #[serde(default)]
    pub due_date: Option<NaiveDate>,
    #[serde(default)]
    pub receipt_no: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateSaleRequest {
    #[serde(default)]
    pub sale_date: Option<NaiveDate>,
    #[serde(default)]
    pub due_date: Option<Option<NaiveDate>>,
    #[serde(default)]
    pub receipt_no: Option<Option<String>>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddLineRequest {
    pub product_id: i64,
    pub qty: Decimal,
    #[serde(default)]
    pub unit_price: Option<Decimal>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateLineRequest {
    pub qty: Decimal,
    pub unit_price: Decimal,
}

#[derive(Debug, Deserialize)]
pub struct RecordPaymentRequest {
    pub method_id: i64,
    pub amount: Decimal,
    pub date: NaiveDate,
}

#[derive(Debug, Deserialize, Default)]
pub struct ConfirmSaleRequest {
    #[serde(default)]
    pub method_id: Option<i64>,
}

#[derive(Debug, Deserialize, Default)]
pub struct CancelSaleRequest {
    #[serde(default)]
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn list_sales(
    State(state): State<AppState>,
    _: Require<SalesRead>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let sales = state.sales_service.list_details().await?;
    Ok(Json(serde_json::json!({ "sales": sales })))
}

async fn sale_debt(
    State(state): State<AppState>,
    _: Require<SalesRead>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let debt = state.sales_service.outstanding_debt().await?;
    Ok(Json(serde_json::json!({ "debt": debt })))
}

async fn create_sale(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Json(payload): Json<CreateSaleRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let customer_id = payload
        .customer_id
        .ok_or_else(|| AppError::Validation("customer_id is required".into()))?;
    let sale = state
        .sales_service
        .create_draft(
            principal.user_id,
            crate::models::NewSale {
                customer_id,
                payment_type: payload.payment_type,
                sale_date: payload.sale_date,
                due_date: payload.due_date,
                receipt_no: payload.receipt_no,
                notes: payload.notes,
            },
        )
        .await?;
    let detail = state.sales_service.get_detail(sale.id).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(detail))))
}

async fn get_sale(
    State(state): State<AppState>,
    _: Require<SalesRead>,
    Path(id): Path<i64>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let detail = state.sales_service.get_detail(id).await?;
    Ok(Json(serde_json::json!(detail)))
}

async fn update_sale(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateSaleRequest>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    state
        .sales_service
        .update_draft(
            id,
            principal.user_id,
            UpdateSaleDraft {
                sale_date: payload.sale_date,
                due_date: payload.due_date,
                receipt_no: payload.receipt_no,
                notes: payload.notes,
            },
        )
        .await?;
    let detail = state.sales_service.get_detail(id).await?;
    Ok(Json(serde_json::json!(detail)))
}

async fn add_line(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    Path(id): Path<i64>,
    Json(payload): Json<AddLineRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let line = state
        .sales_service
        .add_line(id, payload.product_id, payload.qty, payload.unit_price)
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(line))))
}

async fn update_line(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    Path(line_id): Path<i64>,
    Json(payload): Json<UpdateLineRequest>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let line = state
        .sales_service
        .update_line(line_id, payload.qty, payload.unit_price)
        .await?;
    Ok(Json(serde_json::json!(line)))
}

async fn remove_line(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    Path(line_id): Path<i64>,
) -> crate::error::AppResult<StatusCode> {
    state.sales_service.remove_line(line_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// The code names the capability, not the screen: a payment on a confirmed
// sale is money received against an owed balance — a collection — so the gate
// is `customers.collect` and not `sales.create`. The cost is written into the
// S6 mapping table: a `sales.create`-only principal cannot register a payment
// on the sale it recorded; no seeded matrix separates the two codes (both
// `vendedor` and `cajero` hold the pair), so no natural operator is hit.
async fn record_payment(
    State(state): State<AppState>,
    _: Require<CustomersCollect>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
    Json(payload): Json<RecordPaymentRequest>,
) -> crate::error::AppResult<(StatusCode, Json<serde_json::Value>)> {
    let payment = state
        .sales_service
        .record_payment(principal.user_id, id, payload.method_id, payload.amount, payload.date)
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(payment))))
}

// Confirming IS completing the sale the principal recorded: the cash tender
// it embeds is part of the sale itself (one Income + payment row created by
// the same lifecycle step), so the gate stays `sales.create` even though the
// embedded tender posts money — the standalone payment endpoints are where
// `customers.collect` draws the line.
async fn confirm_sale(
    State(state): State<AppState>,
    _: Require<SalesCreate>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
    Json(payload): Json<ConfirmSaleRequest>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let detail = state.sales_service.confirm(principal.user_id, id, payload.method_id).await?;
    Ok(Json(serde_json::json!(detail)))
}

async fn cancel_sale(
    State(state): State<AppState>,
    _: Require<SalesCancel>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
    Json(payload): Json<CancelSaleRequest>,
) -> crate::error::AppResult<Json<serde_json::Value>> {
    let detail = state
        .sales_service
        .cancel(principal.user_id, id, payload.reason)
        .await?;
    Ok(Json(serde_json::json!(detail)))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/sales", get(list_sales).post(create_sale))
        .route("/api/sales/debt", get(sale_debt))
        .route("/api/sales/{id}", get(get_sale).put(update_sale))
        .route("/api/sales/{id}/lines", post(add_line))
        .route(
            "/api/sales/lines/{line_id}",
            put(update_line).delete(remove_line),
        )
        .route("/api/sales/{id}/payments", post(record_payment))
        .route("/api/sales/{id}/confirm", post(confirm_sale))
        .route("/api/sales/{id}/cancel", post(cancel_sale))
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

    async fn seed_customer(
        pool: &sqlx::SqlitePool,
        name: &str,
        limit: Option<&str>,
        payment_days: Option<i64>,
    ) -> i64 {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO customers (name, credit_limit, payment_days, created_by) \
             VALUES (?, ?, ?, ?) RETURNING id",
        )
        .bind(name)
        .bind(limit)
        .bind(payment_days)
        .bind(test_support::audit_actor_id(pool).await.unwrap())
        .fetch_one(pool)
        .await
        .unwrap();
        row.0
    }

    async fn draft_body(
        pool: &sqlx::SqlitePool,
        customer: &str,
        payment_type: &str,
    ) -> serde_json::Value {
        let customer_id = seed_customer(pool, customer, None, None).await;
        let (due_date, sale_date) = ("2024-06-01", "2024-05-02");
        if payment_type == "Credit" {
            serde_json::json!({
                "customer_id": customer_id, "payment_type": payment_type,
                "sale_date": sale_date, "due_date": due_date
            })
        } else {
            serde_json::json!({
                "customer_id": customer_id, "payment_type": payment_type,
                "sale_date": sale_date
            })
        }
    }

    // -- S6 enforcement (AC10): the permission gates on the real handlers ------

    /// Same as [`post_json`]/[`get_json`], but with an explicit cookie: `None`
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

    /// One draft sale with a line behind real stock, created by the shared
    /// full-permission principal, so a limited probe can be refused acting
    /// on it. Returns (sale_id, line_id).
    async fn draft_sale_with_line(
        app: &axum::Router,
        pool: &sqlx::SqlitePool,
        sku: &str,
    ) -> (i64, i64) {
        let pid = seed_product(app, sku, "Product").await;
        seed_stock(app, pid, "100").await;
        let body = draft_body(pool, "Ana Draft", "Credit").await;
        let (st, v) =
            send_json_as(app.clone(), "POST", "/api/sales", Some(test_support::TEST_COOKIE), Some(body))
                .await;
        assert_eq!(st, StatusCode::CREATED, "seed draft sale: {v}");
        let sale_id = v["sale"]["id"].as_i64().unwrap();
        let (st, v) = send_json_as(
            app.clone(),
            "POST",
            &format!("/api/sales/{sale_id}/lines"),
            Some(test_support::TEST_COOKIE),
            Some(serde_json::json!({ "product_id": pid, "qty": "2" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed line: {v}");
        let line_id = v["id"].as_i64().unwrap();
        (sale_id, line_id)
    }

    /// A principal holding ONLY `sales.read` reads everything and is refused
    /// every mutation, each naming its own code: the draft lifecycle is
    /// `sales.create`, cancelling is `sales.cancel`, and money received on a
    /// sale is the collection capability `customers.collect`.
    #[tokio::test]
    async fn ac10_a_sales_read_only_principal_reads_and_is_refused_the_writes() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let (sale, line) = draft_sale_with_line(&app, &pool, "S6-RO").await;
        let probe = test_support::seed_session_with_permissions(&pool, &["sales.read"])
            .await
            .unwrap();
        let probe_cookie = test_support::cookie_for(&probe);

        // The reads the probe is allowed.
        let (st, _) = send_json_as(app.clone(), "GET", "/api/sales", Some(&probe_cookie), None).await;
        assert_eq!(st, StatusCode::OK, "sales.read must open the list");
        let (st, _) = send_json_as(app.clone(), "GET", "/api/sales/debt", Some(&probe_cookie), None).await;
        assert_eq!(st, StatusCode::OK, "sales.read must open the debt report");
        let (st, _) = send_json_as(
            app.clone(),
            "GET",
            &format!("/api/sales/{sale}"),
            Some(&probe_cookie),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "sales.read must open the detail");

        // Draft creation and header edit: sales.create.
        let (st, v) = send_json_as(
            app.clone(),
            "POST",
            "/api/sales",
            Some(&probe_cookie),
            Some(draft_body(&pool, "Denied Create", "Credit").await),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("sales.create"),
            "the refusal must name sales.create: {v}"
        );
        let (st, v) = send_json_as(
            app.clone(),
            "PUT",
            &format!("/api/sales/{sale}"),
            Some(&probe_cookie),
            Some(serde_json::json!({ "notes": "hacked" })),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("sales.create"),
            "the refusal must name sales.create: {v}"
        );

        // Lines: the same draft-recording gate.
        let (st, v) = send_json_as(
            app.clone(),
            "POST",
            &format!("/api/sales/{sale}/lines"),
            Some(&probe_cookie),
            Some(serde_json::json!({ "product_id": 1, "qty": "1" })),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("sales.create"),
            "the refusal must name sales.create: {v}"
        );
        let (st, v) = send_json_as(
            app.clone(),
            "PUT",
            &format!("/api/sales/lines/{line}"),
            Some(&probe_cookie),
            Some(serde_json::json!({ "qty": "9", "unit_price": "9" })),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("sales.create"),
            "the refusal must name sales.create: {v}"
        );
        let (st, v) = send_json_as(
            app.clone(),
            "DELETE",
            &format!("/api/sales/lines/{line}"),
            Some(&probe_cookie),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");

        // Confirm: still the recording tier (the embedded cash tender is part
        // of the sale itself).
        let (st, v) = send_json_as(
            app.clone(),
            "POST",
            &format!("/api/sales/{sale}/confirm"),
            Some(&probe_cookie),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("sales.create"),
            "the refusal must name sales.create: {v}"
        );

        // Cancel: its own tier.
        let (st, v) = send_json_as(
            app.clone(),
            "POST",
            &format!("/api/sales/{sale}/cancel"),
            Some(&probe_cookie),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("sales.cancel"),
            "the refusal must name sales.cancel: {v}"
        );

        // Payment on a sale: money received = a collection, customers.collect.
        let (st, v) = send_json_as(
            app,
            "POST",
            &format!("/api/sales/{sale}/payments"),
            Some(&probe_cookie),
            Some(serde_json::json!({ "method_id": 1, "amount": "5", "date": "2024-05-03" })),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("customers.collect"),
            "the refusal must name customers.collect: {v}"
        );
    }

    /// The refusal writes nothing: the refused draft creation leaves the sales
    /// table where it was, the refused payment leaves no payment row behind
    /// the confirmed sale, and a refused cancel keeps the status.
    #[tokio::test]
    async fn ac10_a_sales_refusal_writes_nothing() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let (sale, _line) = draft_sale_with_line(&app, &pool, "S6-NOWRITE").await;
        let probe = test_support::seed_session_with_permissions(
            &pool,
            &["sales.read", "customers.read"],
        )
        .await
        .unwrap();
        let probe_cookie = test_support::cookie_for(&probe);

        let sales_before: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM sales").fetch_one(&pool).await.unwrap();
        let (st, v) = send_json_as(
            app.clone(),
            "POST",
            "/api/sales",
            Some(&probe_cookie),
            Some(draft_body(&pool, "Denied Write", "Credit").await),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        let sales_after: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM sales").fetch_one(&pool).await.unwrap();
        assert_eq!(sales_after, sales_before, "a refused create must write nothing");

        // Confirm the sale as the shared principal, then refuse a payment.
        let (st, v) = send_json_as(
            app.clone(),
            "POST",
            &format!("/api/sales/{sale}/confirm"),
            Some(test_support::TEST_COOKIE),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "confirm: {v}");
        let payments_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sale_payments")
            .fetch_one(&pool)
            .await
            .unwrap();
        let (st, v) = send_json_as(
            app.clone(),
            "POST",
            &format!("/api/sales/{sale}/payments"),
            Some(&probe_cookie),
            Some(serde_json::json!({ "method_id": 1, "amount": "5", "date": "2024-05-03" })),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        let payments_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sale_payments")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(payments_after, payments_before, "a refused payment must write nothing");

        // A refused cancel leaves the sale Confirmed.
        let (st, v) = send_json_as(
            app.clone(),
            "POST",
            &format!("/api/sales/{sale}/cancel"),
            Some(&probe_cookie),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        let (st, v) = send_json_as(
            app,
            "GET",
            &format!("/api/sales/{sale}"),
            Some(test_support::TEST_COOKIE),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["sale"]["status"], "Confirmed", "a refused cancel must not flip the status");
    }

    /// A principal holding the permissions gets the normal answers: the
    /// confirmed sale accepts its standalone payment and its cancel.
    #[tokio::test]
    async fn ac10_the_sales_holding_principal_gets_the_normal_answer() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let holder = test_support::seed_session_with_permissions(
            &pool,
            &[
                "sales.read",
                "sales.create",
                "sales.cancel",
                "customers.read",
                "customers.collect",
            ],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&holder);
        let (sale, _line) = draft_sale_with_line(&app, &pool, "S6-HOLDER").await;
        let acc = seed_account(&app, "cajaS6").await;
        let cash = allow_cash(&pool, acc).await;

        // Confirm: sales.create answers its normal 200.
        let (st, v) = send_json_as(
            app.clone(),
            "POST",
            &format!("/api/sales/{sale}/confirm"),
            Some(&cookie),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "confirm: {v}");

        // Payment: customers.collect answers its normal 201.
        let (st, v) = send_json_as(
            app.clone(),
            "POST",
            &format!("/api/sales/{sale}/payments"),
            Some(&cookie),
            Some(serde_json::json!({ "method_id": cash, "amount": "10", "date": "2024-05-03" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "payment: {v}");

        // Cancel: sales.cancel answers its normal 200.
        let (st, v) = send_json_as(
            app,
            "POST",
            &format!("/api/sales/{sale}/cancel"),
            Some(&cookie),
            Some(serde_json::json!({})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "cancel: {v}");
        assert_eq!(v["sale"]["status"], "Cancelled");
    }

    /// The gate order must not change: an anonymous request gets the JSON
    /// unauthorized gate, never the permission refusal.
    #[tokio::test]
    async fn an_anonymous_request_still_gets_the_json_gate_not_the_permission_refusal() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let (st, v) = send_json_as(app, "GET", "/api/sales", None, None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
        assert_eq!(v["error"], "unauthorized");
    }

    /// The read gates are real too: a principal WITHOUT `sales.read` (it holds
    /// an unrelated permission, so this is not a broken fixture) is refused
    /// every sales read.
    #[tokio::test]
    async fn the_read_gates_refuse_a_principal_without_the_read_permission() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let (sale, _line) = draft_sale_with_line(&app, &pool, "S6-NOREAD").await;
        let probe = test_support::seed_session_with_permissions(&pool, &["customers.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);

        for uri in ["/api/sales", "/api/sales/debt", &format!("/api/sales/{sale}")] {
            let (st, v) = send_json_as(app.clone(), "GET", uri, Some(&cookie), None).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{uri}: {v}");
            assert!(
                v["error"].as_str().unwrap_or_default().contains("sales.read"),
                "{uri} must name sales.read: {v}"
            );
        }
    }

    // -- AC8: sale_number UNIQUE, immutable, NULL only in Draft/Cancelled-from-Draft --
    #[tokio::test]
    async fn ac8_sale_number_unique_immutable_null_only_draft_via_rest() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let pid = seed_product(&app, "AC8-P", "Product").await;
        seed_stock(&app, pid, "10").await;
        let acc = seed_account(&app, "caja8").await;
        let cash = allow_cash(&pool, acc).await;

        // Draft has NULL sale_number.
        let (st, v) = post_json(app.clone(), "/api/sales", draft_body(&pool, "Ana", "Cash").await).await;
        assert_eq!(st, StatusCode::CREATED, "create draft: {v}");
        let aid = v
            .get("sale")
            .and_then(|s| s.get("id"))
            .or_else(|| v.get("id"))
            .and_then(|x| x.as_i64())
            .unwrap();
        let (st, v) = get_json(app.clone(), &format!("/api/sales/{aid}")).await;
        assert_eq!(st, StatusCode::OK, "get draft: {v}");
        assert!(v.get("sale_number").is_none() || v["sale_number"].is_null());

        // Add line + confirm -> number assigned.
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/sales/{aid}/lines"),
            serde_json::json!({ "product_id": pid, "qty": "1" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, v) = post_json(
            app.clone(),
            &format!("/api/sales/{aid}/confirm"),
            serde_json::json!({ "method_id": cash }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "confirm: {v}");
        let number_a = v
            .get("sale")
            .and_then(|s| s.get("sale_number"))
            .or_else(|| v.get("sale_number"))
            .and_then(|x| x.as_str())
            .unwrap()
            .to_string();
        assert!(number_a.starts_with("2024-SALE-"), "got {number_a}");

        // Second confirm -> UNIQUE second number.
        let (st, v) = post_json(app.clone(), "/api/sales", draft_body(&pool, "Beto", "Cash").await).await;
        assert_eq!(st, StatusCode::CREATED, "create draft b: {v}");
        let bid = v
            .get("sale")
            .and_then(|s| s.get("id"))
            .or_else(|| v.get("id"))
            .and_then(|x| x.as_i64())
            .unwrap();
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/sales/{bid}/lines"),
            serde_json::json!({ "product_id": pid, "qty": "1" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, v) = post_json(
            app.clone(),
            &format!("/api/sales/{bid}/confirm"),
            serde_json::json!({ "method_id": cash }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "confirm b: {v}");
        let number_b = v
            .get("sale")
            .and_then(|s| s.get("sale_number"))
            .or_else(|| v.get("sale_number"))
            .and_then(|x| x.as_str())
            .unwrap()
            .to_string();
        assert_ne!(number_a, number_b);

        // Immutable: edit Confirmed -> 400, number unchanged.
        let (st, _) = put_json(
            app.clone(),
            &format!("/api/sales/{aid}"),
            serde_json::json!({ "notes": "Otro" }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, v) = get_json(app.clone(), &format!("/api/sales/{aid}")).await;
        assert_eq!(st, StatusCode::OK);
        let still = v
            .get("sale")
            .and_then(|s| s.get("sale_number"))
            .or_else(|| v.get("sale_number"))
            .and_then(|x| x.as_str())
            .unwrap();
        assert_eq!(still, number_a);

        // Draft -> Cancelled keeps NULL number (no-op).
        let (st, v) = post_json(app.clone(), "/api/sales", draft_body(&pool, "Ceci", "Cash").await).await;
        assert_eq!(st, StatusCode::CREATED, "create draft c: {v}");
        let cid = v
            .get("sale")
            .and_then(|s| s.get("id"))
            .or_else(|| v.get("id"))
            .and_then(|x| x.as_i64())
            .unwrap();
        let (st, v) = post_json(
            app.clone(),
            &format!("/api/sales/{cid}/cancel"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "cancel draft: {v}");
        let num = v
            .get("sale")
            .and_then(|s| s.get("sale_number"))
            .or_else(|| v.get("sale_number"));
        assert!(num.is_none() || num.unwrap().is_null(), "got {v}");
    }

    // -- AC9: service lines sellable without stock movement --
    #[tokio::test]
    async fn ac9_service_lines_sellable_without_stock_via_rest() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let sid = seed_product(&app, "AC9-SRV", "Service").await;
        let acc = seed_account(&app, "caja9").await;
        let cash = allow_cash(&pool, acc).await;

        let moves_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM stock_movements")
            .fetch_one(&pool)
            .await
            .unwrap();
        let (st, v) = post_json(app.clone(), "/api/sales", draft_body(&pool, "Serv", "Cash").await).await;
        assert_eq!(st, StatusCode::CREATED, "create draft: {v}");
        let id = v
            .get("sale")
            .and_then(|s| s.get("id"))
            .or_else(|| v.get("id"))
            .and_then(|x| x.as_i64())
            .unwrap();
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/sales/{id}/lines"),
            serde_json::json!({ "product_id": sid, "qty": "2" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, v) = post_json(
            app.clone(),
            &format!("/api/sales/{id}/confirm"),
            serde_json::json!({ "method_id": cash }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "confirm service sale: {v}");
        let moves_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM stock_movements")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(moves_before.0, moves_after.0, "service lines move no stock");
        let txs: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(txs.0, 1);
    }

    // -- AC10: finance/stock rows only via services, reference = sale_number --
    #[tokio::test]
    async fn ac10_finance_stock_via_services_reference_sale_number() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let pid = seed_product(&app, "AC10-P", "Product").await;
        seed_stock(&app, pid, "10").await;
        let acc = seed_account(&app, "caja10").await;
        let cash = allow_cash(&pool, acc).await;

        let (st, v) = post_json(app.clone(), "/api/sales", draft_body(&pool, "Ref", "Cash").await).await;
        assert_eq!(st, StatusCode::CREATED, "create draft: {v}");
        let id = v
            .get("sale")
            .and_then(|s| s.get("id"))
            .or_else(|| v.get("id"))
            .and_then(|x| x.as_i64())
            .unwrap();
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/sales/{id}/lines"),
            serde_json::json!({ "product_id": pid, "qty": "3" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, v) = post_json(
            app.clone(),
            &format!("/api/sales/{id}/confirm"),
            serde_json::json!({ "method_id": cash }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "confirm: {v}");
        let number = v
            .get("sale")
            .and_then(|s| s.get("sale_number"))
            .or_else(|| v.get("sale_number"))
            .and_then(|x| x.as_str())
            .unwrap()
            .to_string();

        // Stock Out references the sale_number with reason Sale.
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT type, reason, reference FROM stock_movements WHERE product_id = ? AND type = 'Out'",
        )
        .bind(pid)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "one Out movement: {rows:?}");
        assert_eq!(rows[0].1, "Sale");
        assert_eq!(rows[0].2, number);

        // Income references the sale_number.
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT kind, amount, description FROM transactions WHERE account_id = ?",
        )
        .bind(acc)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "one Income: {rows:?}");
        assert_eq!(rows[0].0, "Income");
        assert_eq!(rows[0].2, number);

        // Cancel -> Sale-return In + Expense refund with same reference.
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/sales/{id}/cancel"),
            serde_json::json!({ "reason": "return" }),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT type, reason, reference FROM stock_movements WHERE product_id = ? AND type = 'In' AND reference = ?",
        )
        .bind(pid)
        .bind(&number)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 1, "one Sale-return In: {rows:?}");
        assert_eq!(rows[0].1, "Sale-return");
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT kind, description FROM transactions WHERE description = ?")
                .bind(&number)
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows.len(), 2, "Income + Expense refund: {rows:?}");
        assert!(rows.iter().any(|r| r.0 == "Expense"));
    }

    // -- Debt endpoint: outstanding Confirmed credit appears, paid leaves --
    #[tokio::test]
    async fn debt_endpoint_lists_unpaid_confirmed_only() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let pid = seed_product(&app, "DEBT-P", "Product").await;
        seed_stock(&app, pid, "10").await;
        let acc = seed_account(&app, "cajaD").await;
        let cash = allow_cash(&pool, acc).await;

        let (st, v) = get_json(app.clone(), "/api/sales/debt").await;
        assert_eq!(st, StatusCode::OK, "empty debt: {v}");

        let (st, v) = post_json(app.clone(), "/api/sales", draft_body(&pool, "Deudor", "Credit").await).await;
        assert_eq!(st, StatusCode::CREATED, "create draft: {v}");
        let id = v
            .get("sale")
            .and_then(|s| s.get("id"))
            .or_else(|| v.get("id"))
            .and_then(|x| x.as_i64())
            .unwrap();
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/sales/{id}/lines"),
            serde_json::json!({ "product_id": pid, "qty": "2" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/sales/{id}/confirm"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let (st, v) = get_json(app.clone(), "/api/sales/debt").await;
        assert_eq!(st, StatusCode::OK, "debt after confirm: {v}");
        let items = v.get("debt").and_then(|x| x.as_array()).unwrap();
        assert_eq!(items.len(), 1, "one debtor: {v}");

        // Partial pay keeps it listed; full pay removes it.
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/sales/{id}/payments"),
            serde_json::json!({ "method_id": cash, "amount": "5", "date": "2024-05-10" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, v) = get_json(app.clone(), "/api/sales/debt").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v.get("debt").and_then(|x| x.as_array()).unwrap().len(), 1);
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/sales/{id}/payments"),
            serde_json::json!({ "method_id": cash, "amount": "15", "date": "2024-05-11" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, v) = get_json(app.clone(), "/api/sales/debt").await;
        assert_eq!(st, StatusCode::OK, "debt after full pay: {v}");
        assert_eq!(v.get("debt").and_then(|x| x.as_array()).unwrap().len(), 0);

        // Error mapping triangulation via REST.
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/sales/{id}/payments"),
            serde_json::json!({ "method_id": cash, "amount": "1", "date": "2024-05-12" }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "overpay must be 400");
        let (st, _) = post_json(
            app.clone(),
            "/api/sales/99999/confirm",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "unknown sale must be 404");
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/sales/{id}/lines"),
            serde_json::json!({ "product_id": 99999, "qty": "1" }),
        )
        .await;
        // Sale is Confirmed now, so edit is rejected (400); unknown product on a
        // Draft would be 404 — both prove the guard rails through HTTP.
        assert!(
            st == StatusCode::BAD_REQUEST || st == StatusCode::NOT_FOUND,
            "edit confirmed / unknown product: {st}"
        );
    }

    #[tokio::test]
    async fn unassigned_method_rejected_via_rest() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let pid = seed_product(&app, "M-REST", "Product").await;
        seed_stock(&app, pid, "10").await;
        let _acc = seed_account(&app, "m-rest").await;
        let cash = cash_method_id(&pool).await;
        // Cash belongs to no account: confirming with it must be 400 with no
        // side effects.
        let (st, v) = post_json(app.clone(), "/api/sales", draft_body(&pool, "Ana", "Cash").await).await;
        assert_eq!(st, StatusCode::CREATED, "create draft: {v}");
        let id = v
            .get("sale")
            .and_then(|s| s.get("id"))
            .or_else(|| v.get("id"))
            .and_then(|x| x.as_i64())
            .unwrap();
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/sales/{id}/lines"),
            serde_json::json!({ "product_id": pid, "qty": "1" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let tx_before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions")
            .fetch_one(&pool)
            .await
            .unwrap();
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/sales/{id}/confirm"),
            serde_json::json!({ "method_id": cash }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "unassigned method must be 400");
        let tx_after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tx_before.0, tx_after.0, "no finance touch on 400");
    }

    // -- K2: mandatory customer and credit limit over the wire ---------------

    /// AC2: the REST DTO cannot create a sale without a customer; an unknown id is
    /// a 404 and a known one is stored with the name snapshotted from the customer.
    #[tokio::test]
    async fn k2_rest_sale_requires_an_existing_customer() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);

        // Missing customer_id => 400, nothing created.
        let (st, v) = post_json(
            app.clone(),
            "/api/sales",
            serde_json::json!({ "payment_type": "Cash", "sale_date": "2024-05-02" }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "missing customer: {v}");
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sales")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0, "a rejected create must not insert a sale");

        // Unknown customer_id => 404.
        let (st, v) = post_json(
            app.clone(),
            "/api/sales",
            serde_json::json!({
                "customer_id": 99999, "payment_type": "Cash", "sale_date": "2024-05-02"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND, "unknown customer: {v}");

        // Known walk-in customer => 201 with the snapshot.
        let (walkin_id,): (i64,) =
            sqlx::query_as("SELECT id FROM customers WHERE is_walkin = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        let (st, v) = post_json(
            app.clone(),
            "/api/sales",
            serde_json::json!({
                "customer_id": walkin_id, "payment_type": "Cash", "sale_date": "2024-05-02"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "walk-in cash sale: {v}");
        assert_eq!(v["sale"]["customer_id"].as_i64(), Some(walkin_id));
        assert_eq!(
            v["sale"]["customer_name"].as_str(),
            Some("Consumidor final")
        );
    }

    /// AC4: the over-limit confirm is a 400 whose body carries the projected debt.
    #[tokio::test]
    async fn k2_rest_credit_limit_400_carries_projected_figure() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let pid = seed_product(&app, "K2-REST", "Product").await;
        seed_stock(&app, pid, "10").await;
        let customer_id = seed_customer(&pool, "REST Limit", Some("50"), None).await;

        let (st, v) = post_json(
            app.clone(),
            "/api/sales",
            serde_json::json!({
                "customer_id": customer_id, "payment_type": "Credit",
                "sale_date": "2024-05-02", "due_date": "2024-06-01"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "draft: {v}");
        let sale_id = v["sale"]["id"].as_i64().unwrap();
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/sales/{sale_id}/lines"),
            serde_json::json!({ "product_id": pid, "qty": "6" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, v) = post_json(
            app.clone(),
            &format!("/api/sales/{sale_id}/confirm"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "over limit: {v}");
        let msg = v["error"].as_str().unwrap_or_default();
        assert!(msg.contains("60"), "projected figure must be visible: {msg}");
        assert!(msg.contains("50"), "limit must be visible: {msg}");
    }

    /// AC3: a credit sale for the walk-in is a 400 over REST too, and the draft
    /// keeps its status and its NULL sale number.
    #[tokio::test]
    async fn k2_rest_credit_to_walkin_is_rejected() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let pid = seed_product(&app, "K2-WALKIN", "Product").await;
        seed_stock(&app, pid, "10").await;
        let (walkin_id,): (i64,) =
            sqlx::query_as("SELECT id FROM customers WHERE is_walkin = 1")
                .fetch_one(&pool)
                .await
                .unwrap();

        let (st, v) = post_json(
            app.clone(),
            "/api/sales",
            serde_json::json!({
                "customer_id": walkin_id, "payment_type": "Credit",
                "sale_date": "2024-05-02", "due_date": "2024-06-02"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "draft: {v}");
        let sale_id = v["sale"]["id"].as_i64().unwrap();
        let (st, _) = post_json(
            app.clone(),
            &format!("/api/sales/{sale_id}/lines"),
            serde_json::json!({ "product_id": pid, "qty": "1" }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);

        let (st, v) = post_json(
            app.clone(),
            &format!("/api/sales/{sale_id}/confirm"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "walk-in credit: {v}");
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains("walk-in"),
            "actionable message: {v}"
        );
        let (st, v) = get_json(app, &format!("/api/sales/{sale_id}")).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["sale"]["status"], "Draft");
        assert!(
            v["sale"]["sale_number"].is_null(),
            "a blocked confirm assigns no number: {v}"
        );
    }
}
