// M4 customers (Slice M). REST for customers, statements, ageing and customer
// receipts. Thin handlers over `CustomerService` (the entity and its rules),
// `SalesService` (the derived receivable: balance, statement, ageing) and
// `CustomerReceiptService` (collect). No SQL here.
//
// Receipts: `collect` derives the receipt from the requested customer and that
// customer's own receivable, so no route accepts a caller-supplied receipt id.
// The only `record_payment_with_receipt(Some(id))` caller is the collection
// service itself, after it verified every covered sale belongs to the receipt's
// customer, so a grouping mismatch is not reachable through the interface and
// the database trigger stays a backstop instead of a 500.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::{AppError, AppResult};
use crate::models::{
    Ageing, Customer, NewCustomer, UpdateCustomer,
};
use crate::routes::AppState;

// S6 enforcement (AC10): the entity and its derived receivable are read with
// `customers.read`, the entity is mutated with `customers.write`, and money
// in — the receipt collect, grouping several invoices — is `customers.collect`.
use crate::security::authz::{CustomersCollect, CustomersRead, CustomersWrite, Require};

// ---------------------------------------------------------------------------
// Request DTOs (JSON, English names)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
pub struct ListCustomersQuery {
    #[serde(default)]
    pub only_active: Option<bool>,
}

/// `as_of` pins the ageing/statement cut-off; absent means today. Tests use it to
/// keep the buckets deterministic. Accepted by both the statement and the
/// receivables view, which age the same receivable.
#[derive(Debug, Deserialize, Default)]
pub struct AsOfQuery {
    #[serde(default)]
    pub as_of: Option<NaiveDate>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ReceiptListQuery {
    #[serde(default)]
    pub customer_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct CreateReceiptRequest {
    pub customer_id: i64,
    pub method_id: i64,
    pub amount: Decimal,
    pub date: NaiveDate,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct UpdateCustomerRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub phone: Option<Option<String>>,
    #[serde(default)]
    pub address: Option<Option<String>>,
    #[serde(default)]
    pub tax_id: Option<Option<String>>,
    #[serde(default)]
    pub notes: Option<Option<String>>,
    #[serde(default)]
    pub credit_limit: Option<Option<Decimal>>,
    #[serde(default)]
    pub payment_days: Option<Option<i64>>,
}

// ---------------------------------------------------------------------------
// Views: the customer entity plus the receivable the routes compose for it
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct CustomerBalanceView {
    pub customer: Customer,
    pub balance: Decimal,
    pub over_limit: bool,
}

#[derive(Debug, Serialize)]
pub struct CustomerAgeingView {
    pub customer_id: i64,
    pub name: String,
    pub balance: Decimal,
    pub over_limit: bool,
    pub ageing: Ageing,
}

/// `credit_limit` is nullable: null means no limit, so the flag can never block.
fn over_limit(customer: &Customer, balance: Decimal) -> bool {
    customer
        .credit_limit
        .map(|limit| balance > limit)
        .unwrap_or(false)
}

fn today() -> NaiveDate {
    chrono::Local::now().date_naive()
}

/// All customers with the derived receivable folded in. `ageing_all` already
/// returns exactly the customers with a non-zero balance, so absent entries are
/// a zero balance and an empty ageing.
async fn customer_views(
    state: &AppState,
    only_active: bool,
) -> AppResult<Vec<CustomerBalanceView>> {
    let ageing = state.sales_service.ageing_all(today()).await?;
    let balances: HashMap<i64, Decimal> = ageing
        .iter()
        .map(|row| (row.customer_id, row.balance))
        .collect();
    let customers = state.customer_service.list_customers(only_active).await?;
    Ok(customers
        .into_iter()
        .map(|customer| {
            let balance = balances.get(&customer.id).copied().unwrap_or(Decimal::ZERO);
            let over_limit = over_limit(&customer, balance);
            CustomerBalanceView {
                customer,
                balance,
                over_limit,
            }
        })
        .collect())
}

async fn customer_view(state: &AppState, customer: Customer) -> AppResult<CustomerBalanceView> {
    let balance = state.sales_service.customer_balance(customer.id).await?;
    let over_limit = over_limit(&customer, balance);
    Ok(CustomerBalanceView {
        customer,
        balance,
        over_limit,
    })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn list_customers(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    Query(query): Query<ListCustomersQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let customers = customer_views(&state, query.only_active.unwrap_or(false)).await?;
    Ok(Json(serde_json::json!({ "customers": customers })))
}

/// A duplicate name is not an error: the response carries the existing matches
/// so the interface can warn without blocking (AC15).
async fn create_customer(
    State(state): State<AppState>,
    _: Require<CustomersWrite>,
    Json(payload): Json<NewCustomer>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let created = state.customer_service.create_customer(payload).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(created))))
}

async fn get_customer(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let customer = state.customer_service.get_customer(id).await?;
    let view = customer_view(&state, customer).await?;
    Ok(Json(serde_json::json!(view)))
}

async fn update_customer(
    State(state): State<AppState>,
    _: Require<CustomersWrite>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateCustomerRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let customer = state
        .customer_service
        .update_customer(
            id,
            UpdateCustomer {
                name: payload.name,
                phone: payload.phone,
                address: payload.address,
                tax_id: payload.tax_id,
                notes: payload.notes,
                credit_limit: payload.credit_limit,
                payment_days: payload.payment_days,
            },
        )
        .await?;
    let view = customer_view(&state, customer).await?;
    Ok(Json(serde_json::json!(view)))
}

async fn activate_customer(
    State(state): State<AppState>,
    _: Require<CustomersWrite>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let customer = state.customer_service.activate_customer(id).await?;
    let view = customer_view(&state, customer).await?;
    Ok(Json(serde_json::json!(view)))
}

async fn deactivate_customer(
    State(state): State<AppState>,
    _: Require<CustomersWrite>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let customer = state.customer_service.deactivate_customer(id).await?;
    let view = customer_view(&state, customer).await?;
    Ok(Json(serde_json::json!(view)))
}

async fn delete_customer(
    State(state): State<AppState>,
    _: Require<CustomersWrite>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    state.customer_service.delete_customer(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Statement with the ageing breakdown and the chronological ledger, composed
/// with the customer the module owns.
// The statement renders THAT customer's own documents (sales, payments,
// receipts) as the receivable ledger: the module owns the customer's account
// view, so the single gate is `customers.read`. The cost is written into the
// S6 mapping table: a customers.read-only principal sees the sale documents of
// that customer (number/date/total) — data the receivable is meaningless
// without — but never the sales list or other customers' sales.
async fn customer_statement(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    Path(id): Path<i64>,
    Query(query): Query<AsOfQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let customer = state.customer_service.get_customer(id).await?;
    let statement = state
        .sales_service
        .customer_statement(id, query.as_of.unwrap_or_else(today))
        .await?;
    Ok(Json(
        serde_json::json!({ "customer": customer, "statement": statement }),
    ))
}

/// Receivables view: every customer with a non-zero balance and the ageing of
/// that balance.
async fn customer_ageing(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    Query(query): Query<AsOfQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let as_of = query.as_of.unwrap_or_else(today);
    let rows = state.sales_service.ageing_all(as_of).await?;
    let customers: HashMap<i64, Customer> = state
        .customer_service
        .list_customers(false)
        .await?
        .into_iter()
        .map(|customer| (customer.id, customer))
        .collect();
    let views: Vec<CustomerAgeingView> = rows
        .into_iter()
        .filter_map(|row| {
            let customer = customers.get(&row.customer_id)?;
            Some(CustomerAgeingView {
                customer_id: row.customer_id,
                name: customer.name.clone(),
                balance: row.balance,
                over_limit: over_limit(customer, row.balance),
                ageing: row.ageing,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "ageing": views })))
}

// ---------------------------------------------------------------------------
// Receipts: list by customer, one with its allocations, and collect
// ---------------------------------------------------------------------------

/// Receipts of one customer, each with the payments it groups. There is no
/// unbounded receipt dump: the customer is mandatory.
async fn list_receipts(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    Query(query): Query<ReceiptListQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let customer_id = query
        .customer_id
        .ok_or_else(|| AppError::Validation("customer_id is required".into()))?;
    state.customer_service.get_customer(customer_id).await?;
    let receipts = state.customer_receipt_service.list_receipts(customer_id).await?;
    Ok(Json(serde_json::json!({ "receipts": receipts })))
}

async fn get_receipt(
    State(state): State<AppState>,
    _: Require<CustomersRead>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let detail = state.customer_receipt_service.get_receipt(id).await?;
    Ok(Json(serde_json::json!(detail)))
}

/// Collect one handover of money. The receipt is derived here from the customer
/// and that customer's own receivable; the request has no receipt id, so the
/// caller cannot group a payment under someone else's document.
async fn collect_receipt(
    State(state): State<AppState>,
    _: Require<CustomersCollect>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Json(payload): Json<CreateReceiptRequest>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let detail = state
        .customer_receipt_service
        .collect(
            principal.user_id,
            payload.customer_id,
            payload.method_id,
            payload.amount,
            payload.date,
            payload.notes,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(detail))))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/customers", get(list_customers).post(create_customer))
        .route("/api/customers/ageing", get(customer_ageing))
        .route(
            "/api/customers/{id}",
            get(get_customer).put(update_customer).delete(delete_customer),
        )
        .route("/api/customers/{id}/statement", get(customer_statement))
        .route("/api/customers/{id}/activate", post(activate_customer))
        .route("/api/customers/{id}/deactivate", post(deactivate_customer))
        .route(
            "/api/customer-receipts",
            get(list_receipts).post(collect_receipt),
        )
        .route("/api/customer-receipts/{id}", get(get_receipt))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use rust_decimal::Decimal;
    use serde_json::{json, Value};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::SqlitePool;
    use std::str::FromStr;
    use tower::ServiceExt;

    use crate::routes::AppState;
    use crate::security::test_support;

    async fn test_state(enforce_credit_limit: bool) -> AppState {
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
        AppState::new_with_credit_limit(pool, false, true, enforce_credit_limit)
    }

    async fn request(
        app: &axum::Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut builder = test_support::with_cookie(Request::builder().method(method).uri(uri));
        let payload = match body {
            Some(v) => {
                builder = builder.header("content-type", "application/json");
                v.to_string()
            }
            None => String::new(),
        };
        let resp = app
            .clone()
            .oneshot(builder.body(Body::from(payload)).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
        request(app, "GET", uri, None).await
    }

    async fn post(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
        request(app, "POST", uri, Some(body)).await
    }

    async fn put(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
        request(app, "PUT", uri, Some(body)).await
    }

    async fn delete(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
        request(app, "DELETE", uri, None).await
    }

    fn dec(v: &Value) -> Decimal {
        let raw = v
            .as_str()
            .unwrap_or_else(|| panic!("expected a decimal string, got {v}"));
        Decimal::from_str(raw).unwrap_or_else(|e| panic!("invalid decimal {raw}: {e}"))
    }

    async fn seed_product(app: &axum::Router, sku: &str) -> i64 {
        let (st, v) = post(
            app,
            "/api/products",
            json!({
                "sku": sku, "name": format!("prod {sku}"), "kind": "Product",
                "unit": "un", "sale_price": "10", "cost_price": "5",
                "track_stock": true, "min_stock": "0", "max_stock": "50"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed product {sku}: {v}");
        v["id"].as_i64().unwrap()
    }

    async fn seed_stock(app: &axum::Router, product_id: i64) {
        let (st, v) = post(
            app,
            "/api/stock-movements",
            json!({
                "product_id": product_id, "qty": "100", "type": "In",
                "reason": "Initial", "date": "2024-05-01"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed stock: {v}");
    }

    async fn seed_account(app: &axum::Router, name: &str) -> i64 {
        let (st, v) = post(app, "/api/accounts", json!({ "name": name })).await;
        assert_eq!(st, StatusCode::CREATED, "seed account {name}: {v}");
        v["id"].as_i64().unwrap()
    }

    async fn allow_cash(pool: &SqlitePool, account_id: i64) -> i64 {
        let (cash,): (i64,) =
            sqlx::query_as("SELECT id FROM payment_methods WHERE name = 'Cash'")
                .fetch_one(pool)
                .await
                .unwrap();
        // Ownership, not an allowlist: assign the unassigned Cash, or duplicate
        // the name when it is already owned elsewhere in this pool.
        let assigned = sqlx::query(
            "UPDATE payment_methods SET account_id = ? WHERE id = ? AND account_id IS NULL",
        )
        .bind(account_id)
        .bind(cash)
        .execute(pool)
        .await
        .unwrap()
        .rows_affected();
        if assigned == 1 {
            return cash;
        }
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO payment_methods (name, account_id, is_active, created_by) \
             SELECT name, ?, is_active, ? FROM payment_methods WHERE id = ? RETURNING id",
        )
        .bind(account_id)
        .bind(test_support::audit_actor_id(pool).await.unwrap())
        .bind(cash)
        .fetch_one(pool)
        .await
        .unwrap();
        row.0
    }

    async fn seed_customer(
        app: &axum::Router,
        name: &str,
        limit: Option<&str>,
        payment_days: Option<i64>,
    ) -> i64 {
        let mut body = json!({ "name": name });
        if let Some(limit) = limit {
            body["credit_limit"] = json!(limit);
        }
        if let Some(days) = payment_days {
            body["payment_days"] = json!(days);
        }
        let (st, v) = post(app, "/api/customers", body).await;
        assert_eq!(st, StatusCode::CREATED, "seed customer {name}: {v}");
        v["customer"]["id"].as_i64().unwrap()
    }

    async fn credit_sale(
        app: &axum::Router,
        customer_id: i64,
        product_id: i64,
        qty: &str,
        due_date: &str,
    ) -> i64 {
        let (st, v) = post(
            app,
            "/api/sales",
            json!({
                "customer_id": customer_id, "payment_type": "Credit",
                "sale_date": "2024-05-02", "due_date": due_date
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed sale: {v}");
        let sale_id = v["sale"]["id"].as_i64().unwrap();
        let (st, v) = post(
            app,
            &format!("/api/sales/{sale_id}/lines"),
            json!({ "product_id": product_id, "qty": qty }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "seed line: {v}");
        let (st, v) = post(app, &format!("/api/sales/{sale_id}/confirm"), json!({})).await;
        assert_eq!(st, StatusCode::OK, "confirm credit sale: {v}");
        sale_id
    }

    async fn receipt_count(pool: &SqlitePool) -> i64 {
        sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM customer_receipts")
            .fetch_one(pool)
            .await
            .unwrap()
            .0
    }

    // -- CRUD, duplicate warning and the walk-in protections -------------------

    #[tokio::test]
    async fn rest_customers_crud_duplicate_warning_and_protections() {
        let state = test_state(true).await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let product = seed_product(&app, "REST-CUST-P").await;
        seed_stock(&app, product).await;

        // Create: trimmed, with a limit and a term; nothing to warn about yet.
        let (st, v) = post(
            &app,
            "/api/customers",
            json!({
                "name": "  Ana Pérez  ", "phone": "  555-1234  ",
                "credit_limit": "100", "payment_days": 30
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "create: {v}");
        let id = v["customer"]["id"].as_i64().unwrap();
        assert_eq!(v["customer"]["name"], json!("Ana Pérez"));
        assert_eq!(v["customer"]["phone"], json!("555-1234"));
        assert_eq!(dec(&v["customer"]["credit_limit"]), dec(&json!("100")));
        assert_eq!(v["customer"]["payment_days"], json!(30));
        assert!(v["name_matches"].as_array().unwrap().is_empty());

        // A duplicate name is accepted and the existing matches are reported.
        let (st, v) = post(&app, "/api/customers", json!({ "name": "Ana Pérez" })).await;
        assert_eq!(st, StatusCode::CREATED, "duplicate: {v}");
        assert_eq!(v["name_matches"].as_array().unwrap().len(), 1);
        assert_eq!(v["name_matches"][0]["id"].as_i64(), Some(id));

        // Get composes the derived balance and the over-limit flag.
        let (st, v) = get(&app, &format!("/api/customers/{id}")).await;
        assert_eq!(st, StatusCode::OK, "get: {v}");
        assert_eq!(dec(&v["balance"]), Decimal::ZERO);
        assert_eq!(v["over_limit"], json!(false));

        // Update patches only what it receives.
        let (st, v) = put(
            &app,
            &format!("/api/customers/{id}"),
            json!({ "name": "Ana P.", "credit_limit": "50" }),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "update: {v}");
        assert_eq!(v["customer"]["name"], json!("Ana P."));
        assert_eq!(dec(&v["customer"]["credit_limit"]), dec(&json!("50")));
        assert_eq!(v["customer"]["payment_days"], json!(30));
        assert!(v["customer"]["is_active"].as_bool().unwrap());

        // Activate/deactivate.
        let (st, v) = post(
            &app,
            &format!("/api/customers/{id}/deactivate"),
            json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "deactivate: {v}");
        assert_eq!(v["customer"]["is_active"], json!(false));

        // The list can be filtered to active customers only.
        let (st, v) = get(&app, "/api/customers?only_active=true").await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert!(
            v["customers"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["customer"]["id"].as_i64() != Some(id)),
            "an inactive customer must not appear in the active list: {v}"
        );
        let (st, v) = get(&app, "/api/customers?only_active=false").await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert!(v["customers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["customer"]["id"].as_i64() == Some(id)));

        let (st, v) = post(&app, &format!("/api/customers/{id}/activate"), json!({})).await;
        assert_eq!(st, StatusCode::OK, "activate: {v}");
        assert_eq!(v["customer"]["is_active"], json!(true));

        // A second walk-in cannot be created; the seeded one is the only one.
        let (st, v) = post(
            &app,
            "/api/customers",
            json!({ "name": "Otro mostrador", "is_walkin": true }),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT, "second walk-in: {v}");

        // A customer with history cannot be deleted; deactivate instead.
        let sale_id = credit_sale(&app, id, product, "1", "2024-06-01").await;
        let (st, v) = delete(&app, &format!("/api/customers/{id}")).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "delete with history: {v}");
        assert!(get(&app, &format!("/api/customers/{id}")).await.0 == StatusCode::OK);
        assert_eq!(
            get(&app, &format!("/api/sales/{sale_id}")).await.0,
            StatusCode::OK
        );

        // A customer without history deletes with 204 and then reads 404.
        let fresh = seed_customer(&app, "Sin historial", None, None).await;
        let (st, _) = delete(&app, &format!("/api/customers/{fresh}")).await;
        assert_eq!(st, StatusCode::NO_CONTENT);
        assert_eq!(
            get(&app, &format!("/api/customers/{fresh}")).await.0,
            StatusCode::NOT_FOUND
        );

        // The walk-in can never be deactivated or deleted.
        let (walkin,): (i64,) =
            sqlx::query_as("SELECT id FROM customers WHERE is_walkin = 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        let (st, v) = post(
            &app,
            &format!("/api/customers/{walkin}/deactivate"),
            json!({}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "walk-in deactivate: {v}");
        let (st, v) = delete(&app, &format!("/api/customers/{walkin}")).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "walk-in delete: {v}");
        assert_eq!(
            get(&app, &format!("/api/customers/{walkin}")).await.0,
            StatusCode::OK
        );

        // Validation and not-found mapping.
        let (st, _) = post(&app, "/api/customers", json!({ "name": "   " })).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _) = post(
            &app,
            "/api/customers",
            json!({ "name": "Negativo", "credit_limit": "-1" }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(get(&app, "/api/customers/999999").await.0, StatusCode::NOT_FOUND);
        assert_eq!(
            delete(&app, "/api/customers/999999").await.0,
            StatusCode::NOT_FOUND
        );
    }

    // -- Statement, ageing and over-limit --------------------------------------

    #[tokio::test]
    async fn rest_statement_ageing_and_over_limit_with_enforcement_off() {
        let state = test_state(false).await;
        let app = crate::routes::router(state);
        let product = seed_product(&app, "REST-STMT-P").await;
        seed_stock(&app, product).await;

        // Limit 40, two debts of 30 + 20 => projected 50 is over the limit, and
        // with enforcement off the sale is confirmed and the customer reads over.
        let customer = seed_customer(&app, "Sobre el límite", Some("40"), None).await;
        credit_sale(&app, customer, product, "3", "2024-06-01").await;
        credit_sale(&app, customer, product, "2", "2024-07-01").await;

        let (st, v) = get(&app, &format!("/api/customers/{customer}")).await;
        assert_eq!(st, StatusCode::OK, "get over-limit: {v}");
        assert_eq!(dec(&v["balance"]), dec(&json!("50")));
        assert_eq!(v["over_limit"], json!(true));

        // The list carries the same derived figures.
        let (st, v) = get(&app, "/api/customers").await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let row = v["customers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["customer"]["id"].as_i64() == Some(customer))
            .unwrap_or_else(|| panic!("customer {customer} missing from list: {v}"));
        assert_eq!(dec(&row["balance"]), dec(&json!("50")));
        assert_eq!(row["over_limit"], json!(true));

        // Statement: chronological ledger and the ageing as of the query date.
        let (st, v) = get(
            &app,
            &format!("/api/customers/{customer}/statement?as_of=2024-06-20"),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "statement: {v}");
        assert_eq!(v["customer"]["id"].as_i64(), Some(customer));
        let statement = &v["statement"];
        assert_eq!(dec(&statement["balance"]), dec(&json!("50")));
        assert_eq!(dec(&statement["ageing"]["overdue_1_30"]), dec(&json!("30")));
        assert_eq!(dec(&statement["ageing"]["current"]), dec(&json!("20")));
        let entries = statement["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "two sales, no payments yet: {entries:?}");
        assert!(entries.iter().all(|e| e["kind"] == json!("Sale")));
        assert_eq!(dec(&entries[1]["balance"]), dec(&json!("50")));

        // The receivables view lists the customer with the same ageing.
        let (st, v) = get(&app, "/api/customers/ageing?as_of=2024-06-20").await;
        assert_eq!(st, StatusCode::OK, "ageing: {v}");
        let row = v["ageing"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["customer_id"].as_i64() == Some(customer))
            .unwrap_or_else(|| panic!("customer {customer} missing from ageing: {v}"));
        assert_eq!(dec(&row["balance"]), dec(&json!("50")));
        assert_eq!(row["over_limit"], json!(true));
        assert_eq!(dec(&row["ageing"]["overdue_1_30"]), dec(&json!("30")));
        assert_eq!(dec(&row["ageing"]["current"]), dec(&json!("20")));
    }

    // -- Collect: receipt total, allocations and reads --------------------------

    #[tokio::test]
    async fn rest_collect_creates_a_receipt_with_derived_total_and_allocations() {
        let state = test_state(true).await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state.clone());
        let product = seed_product(&app, "REST-COLL-P").await;
        seed_stock(&app, product).await;
        let account = seed_account(&app, "Caja").await;
        let cash = allow_cash(&pool, account).await;
        let customer = seed_customer(&app, "Ana Cobros", None, None).await;
        credit_sale(&app, customer, product, "3", "2024-06-01").await; // 30
        credit_sale(&app, customer, product, "5", "2024-07-01").await; // 50

        // Collect 60 oldest first: 30 covers the first sale, 30 eats into the 50.
        let (st, v) = post(
            &app,
            "/api/customer-receipts",
            json!({
                "customer_id": customer, "method_id": cash,
                "amount": "60", "date": "2024-06-20", "notes": "  partial  "
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "collect: {v}");
        let receipt_id = v["receipt"]["id"].as_i64().unwrap();
        assert_eq!(dec(&v["total"]), dec(&json!("60")));
        assert_eq!(v["receipt"]["notes"], json!("partial"));
        let allocations = v["allocations"].as_array().unwrap();
        assert_eq!(allocations.len(), 2, "one allocation per covered sale: {v}");
        let amounts: Vec<Decimal> = allocations.iter().map(|a| dec(&a["amount"])).collect();
        assert_eq!(amounts, vec![dec(&json!("30")), dec(&json!("30"))]);
        for a in allocations {
            assert_eq!(a["receipt_id"].as_i64(), Some(receipt_id));
            assert!(
                a["transaction_id"].as_i64().is_some(),
                "every grouped payment keeps its own finance link: {a}"
            );
        }
        // The receipt total is exactly the sum of its allocations.
        let sum: Decimal = allocations.iter().map(|a| dec(&a["amount"])).sum();
        assert_eq!(sum, dec(&v["total"]));

        // Reads: by id, by customer, and the derived balance afterwards.
        let (st, one) = get(&app, &format!("/api/customer-receipts/{receipt_id}")).await;
        assert_eq!(st, StatusCode::OK, "receipt by id: {one}");
        assert_eq!(one["receipt"]["id"].as_i64(), Some(receipt_id));
        assert_eq!(dec(&one["total"]), dec(&json!("60")));

        let (st, list) = get(
            &app,
            &format!("/api/customer-receipts?customer_id={customer}"),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "receipts by customer: {list}");
        assert_eq!(list["receipts"].as_array().unwrap().len(), 1);
        assert_eq!(
            list["receipts"][0]["receipt"]["id"].as_i64(),
            Some(receipt_id)
        );

        let (st, v) = get(&app, &format!("/api/customers/{customer}")).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(dec(&v["balance"]), dec(&json!("20")));

        // The statement now mixes the sale debits with the payment credit.
        let (st, v) = get(
            &app,
            &format!("/api/customers/{customer}/statement?as_of=2024-07-20"),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let entries = v["statement"]["entries"].as_array().unwrap();
        assert_eq!(
            entries.len(),
            4,
            "two sale debits plus two allocation credits: {entries:?}"
        );
        let credits: Decimal = entries
            .iter()
            .filter(|e| e["kind"] == json!("Payment"))
            .map(|e| dec(&e["credit"]))
            .sum();
        assert_eq!(credits, dec(&json!("60")));
        assert_eq!(dec(&v["statement"]["balance"]), dec(&json!("20")));
    }

    #[tokio::test]
    async fn rest_collect_rejections_leave_no_side_effect() {
        let state = test_state(true).await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let product = seed_product(&app, "REST-REJ-P").await;
        seed_stock(&app, product).await;
        let account = seed_account(&app, "Caja").await;
        let cash = allow_cash(&pool, account).await;
        let customer = seed_customer(&app, "Rechazos", None, None).await;
        credit_sale(&app, customer, product, "3", "2024-06-01").await; // 30

        // Over the outstanding debt.
        let (st, v) = post(
            &app,
            "/api/customer-receipts",
            json!({
                "customer_id": customer, "method_id": cash,
                "amount": "31", "date": "2024-06-20"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "over-collect: {v}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("30"),
            "the outstanding figure must be quoted: {v}"
        );
        assert_eq!(receipt_count(&pool).await, 0);

        // Unassigned method: no account can be derived, so it is a 400.
        sqlx::query("UPDATE payment_methods SET account_id = NULL WHERE id = ?")
            .bind(cash)
            .execute(&pool)
            .await
            .unwrap();
        let (st, v) = post(
            &app,
            "/api/customer-receipts",
            json!({
                "customer_id": customer, "method_id": cash,
                "amount": "10", "date": "2024-06-20"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "unassigned method: {v}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("not assigned"),
            "the message must name the fix: {v}"
        );
        assert_eq!(receipt_count(&pool).await, 0);

        // Unknown customer is a 404 before any write; unknown method too.
        let (st, _) = post(
            &app,
            "/api/customer-receipts",
            json!({
                "customer_id": 999999, "method_id": cash,
                "amount": "10", "date": "2024-06-20"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, _) = post(
            &app,
            "/api/customer-receipts",
            json!({
                "customer_id": customer, "method_id": 999999,
                "amount": "10", "date": "2024-06-20"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(receipt_count(&pool).await, 0);

        // A list without customer_id is a 400, not an unbounded dump.
        let (st, v) = get(&app, "/api/customer-receipts").await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
        assert_eq!(
            get(&app, "/api/customer-receipts/999999").await.0,
            StatusCode::NOT_FOUND
        );

        // The receivable is untouched by every rejection.
        let (_, v) = get(&app, &format!("/api/customers/{customer}")).await;
        assert_eq!(dec(&v["balance"]), dec(&json!("30")));
    }

    // -- The interface never offers a caller-supplied receipt id ----------------

    #[tokio::test]
    async fn rest_payment_route_cannot_group_under_a_callers_receipt() {
        let state = test_state(true).await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let product = seed_product(&app, "REST-MISMATCH-P").await;
        seed_stock(&app, product).await;
        let account = seed_account(&app, "Caja").await;
        let cash = allow_cash(&pool, account).await;
        let ana = seed_customer(&app, "Ana", None, None).await;
        let beto = seed_customer(&app, "Beto", None, None).await;
        let ana_sale = credit_sale(&app, ana, product, "3", "2024-06-01").await; // 30
        let beto_sale = credit_sale(&app, beto, product, "1", "2024-06-01").await; // 10

        // Beto's receipt, produced by collecting Beto's own debt.
        let (st, v) = post(
            &app,
            "/api/customer-receipts",
            json!({
                "customer_id": beto, "method_id": cash,
                "amount": "10", "date": "2024-06-20"
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        let beto_receipt = v["receipt"]["id"].as_i64().unwrap();

        // The direct-payment route has no receipt field: the extra key is ignored
        // exactly like any other unknown JSON field, so Ana's payment is ungrouped
        // and Beto's receipt stays untouched instead of a trigger abort.
        let (st, v) = post(
            &app,
            &format!("/api/sales/{ana_sale}/payments"),
            json!({
                "method_id": cash,
                "amount": "10", "date": "2024-06-20",
                "receipt_id": beto_receipt
            }),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "direct payment: {v}");
        assert!(v["receipt_id"].is_null(), "{v}");
        assert!(v["transaction_id"].as_i64().is_some(), "{v}");

        let (_, receipt) = get(&app, &format!("/api/customer-receipts/{beto_receipt}")).await;
        assert_eq!(dec(&receipt["total"]), dec(&json!("10")));
        assert_eq!(receipt["allocations"].as_array().unwrap().len(), 1);
        assert_eq!(
            receipt["allocations"][0]["sale_id"].as_i64(),
            Some(beto_sale),
            "Beto's receipt still groups only Beto's sale"
        );

        // The grouped payment still resolves through its own sale and movement.
        let (_, ana_detail) = get(&app, &format!("/api/sales/{ana_sale}")).await;
        assert_eq!(ana_detail["payments"].as_array().unwrap().len(), 1);
        assert_eq!(dec(&ana_detail["due"]), dec(&json!("20")));
    }

    // -- S6 enforcement (AC10): the permission gates on the real handlers ------

    /// Like [`request`], but with an explicit cookie: `None` means the truly
    /// anonymous request; a minted token drives a probe principal.
    async fn request_as(
        app: &axum::Router,
        method: &str,
        uri: &str,
        cookie: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        let payload = match body {
            Some(v) => {
                builder = builder.header("content-type", "application/json");
                v.to_string()
            }
            None => String::new(),
        };
        let resp = app
            .clone()
            .oneshot(builder.body(Body::from(payload)).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json)
    }

    /// One confirmed credit sale (50 owed) behind the shared principal, so a
    /// limited probe can be refused collecting against it.
    async fn seeded_receivable(app: &axum::Router) -> (i64, i64, i64) {
        let product = seed_product(app, "S6-CUST-P").await;
        seed_stock(app, product).await;
        let customer = seed_customer(app, "Ana S6", Some("100"), Some(30)).await;
        let sale = credit_sale(app, customer, product, "5", "2024-06-01").await;
        (customer, sale, product)
    }

    /// A principal holding ONLY `customers.read` reads everything — the list,
    /// the ageing, the statement, the receipts — and is refused every entity
    /// mutation (`customers.write`) and every collection (`customers.collect`).
    #[tokio::test]
    async fn ac10_a_customers_read_only_principal_reads_and_is_refused_the_writes() {
        let state = test_state(true).await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let (customer, _sale, _product) = seeded_receivable(&app).await;
        let probe = test_support::seed_session_with_permissions(&pool, &["customers.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);

        // The reads the probe is allowed.
        let (st, _) = request_as(&app, "GET", "/api/customers", Some(&cookie), None).await;
        assert_eq!(st, StatusCode::OK, "customers.read must open the list");
        let (st, _) = request_as(&app, "GET", "/api/customers/ageing", Some(&cookie), None).await;
        assert_eq!(st, StatusCode::OK, "customers.read must open the ageing");
        let (st, _) = request_as(
            &app,
            "GET",
            &format!("/api/customers/{customer}/statement"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "customers.read must open the statement");
        let (st, _) = request_as(
            &app,
            "GET",
            &format!("/api/customer-receipts?customer_id={customer}"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "customers.read must open the receipts");

        // Entity mutations: customers.write.
        let (st, v) = request_as(
            &app,
            "POST",
            "/api/customers",
            Some(&cookie),
            Some(json!({ "name": "Denied Write" })),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("customers.write"),
            "the refusal must name customers.write: {v}"
        );
        let (st, v) = request_as(
            &app,
            "PUT",
            &format!("/api/customers/{customer}"),
            Some(&cookie),
            Some(json!({ "name": "Hacked" })),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("customers.write"),
            "the refusal must name customers.write: {v}"
        );
        for action in ["activate", "deactivate"] {
            let (st, v) = request_as(
                &app,
                "POST",
                &format!("/api/customers/{customer}/{action}"),
                Some(&cookie),
                Some(json!({})),
            )
            .await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{action}: {v}");
            assert!(
                v["error"].as_str().unwrap_or_default().contains("customers.write"),
                "{action} must name customers.write: {v}"
            );
        }
        let (st, v) =
            request_as(&app, "DELETE", &format!("/api/customers/{customer}"), Some(&cookie), None)
                .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("customers.write"),
            "the refusal must name customers.write: {v}"
        );

        // The collection: its own tier, customers.collect.
        let (st, v) = request_as(
            &app,
            "POST",
            "/api/customer-receipts",
            Some(&cookie),
            Some(json!({
                "customer_id": customer, "method_id": 1,
                "amount": "10", "date": "2024-06-02"
            })),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("customers.collect"),
            "the refusal must name customers.collect: {v}"
        );
    }

    /// The refusal writes nothing: the refused collect leaves no receipt row
    /// and no payment/transaction behind, and a refused delete leaves the
    /// customers table (and the receivable) untouched.
    #[tokio::test]
    async fn ac10_a_customers_refusal_writes_nothing() {
        let state = test_state(true).await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let (customer, _sale, _product) = seeded_receivable(&app).await;
        let acc = seed_account(&app, "cajaCust").await;
        let cash = allow_cash(&pool, acc).await;
        let probe = test_support::seed_session_with_permissions(
            &pool,
            &["customers.read"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&probe);

        let customers_before: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM customers").fetch_one(&pool).await.unwrap();
        let (st, v) = request_as(
            &app,
            "POST",
            "/api/customers",
            Some(&cookie),
            Some(json!({ "name": "Denied Write" })),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        let customers_after: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM customers").fetch_one(&pool).await.unwrap();
        assert_eq!(customers_after, customers_before, "a refused create must write nothing");

        let (st, v) =
            request_as(&app, "DELETE", &format!("/api/customers/{customer}"), Some(&cookie), None)
                .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        let (st, v) = request_as(
            &app,
            "GET",
            &format!("/api/customers/{customer}"),
            Some(test_support::TEST_COOKIE),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "the customer must survive the refused delete: {v}");

        let receipts_before: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM customer_receipts").fetch_one(&pool).await.unwrap();
        let (st, v) = request_as(
            &app,
            "POST",
            "/api/customer-receipts",
            Some(&cookie),
            Some(json!({
                "customer_id": customer, "method_id": cash,
                "amount": "10", "date": "2024-06-02"
            })),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("customers.collect"),
            "the refusal must name customers.collect: {v}"
        );
        let receipts_after: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM customer_receipts").fetch_one(&pool).await.unwrap();
        assert_eq!(receipts_after, receipts_before, "a refused collect must write nothing");
    }

    /// A principal holding the permissions gets the normal answers: the
    /// entity round trip and a real collection against the receivable.
    #[tokio::test]
    async fn ac10_the_customers_holding_principal_gets_the_normal_answer() {
        let state = test_state(true).await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let (customer, _sale, _product) = seeded_receivable(&app).await;
        let acc = seed_account(&app, "cajaHold").await;
        let cash = allow_cash(&pool, acc).await;
        let holder = test_support::seed_session_with_permissions(
            &pool,
            &["customers.read", "customers.write", "customers.collect"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&holder);

        // Create and update: customers.write answers its normal codes.
        let (st, v) = request_as(
            &app,
            "POST",
            "/api/customers",
            Some(&cookie),
            Some(json!({ "name": "Beto Holder" })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "create: {v}");
        let (st, v) = request_as(
            &app,
            "PUT",
            &format!("/api/customers/{customer}"),
            Some(&cookie),
            Some(json!({ "phone": "555-9999" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "update: {v}");

        // Collect: customers.collect answers its normal 201 and the balance drops.
        let (st, v) = request_as(
            &app,
            "POST",
            "/api/customer-receipts",
            Some(&cookie),
            Some(json!({
                "customer_id": customer, "method_id": cash,
                "amount": "20", "date": "2024-06-02"
            })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "collect: {v}");
        let (_, v) = request_as(
            &app,
            "GET",
            &format!("/api/customers/{customer}"),
            Some(&cookie),
            None,
        )
        .await;
        assert_eq!(dec(&v["balance"]), dec(&json!("30")), "the collect must drop the balance: {v}");
    }

    /// The gate order must not change: an anonymous request gets the JSON
    /// unauthorized gate, never the permission refusal.
    #[tokio::test]
    async fn an_anonymous_request_still_gets_the_json_gate_not_the_permission_refusal() {
        let state = test_state(true).await;
        let app = crate::routes::router(state);
        let (st, v) = request_as(&app, "GET", "/api/customers", None, None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
        assert_eq!(v["error"], "unauthorized");
    }

    /// The read gates are real too: a principal WITHOUT `customers.read` (it
    /// holds an unrelated permission, so this is not a broken fixture) is
    /// refused every customers read, receipts included.
    #[tokio::test]
    async fn the_read_gates_refuse_a_principal_without_the_read_permission() {
        let state = test_state(true).await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let (customer, _sale, _product) = seeded_receivable(&app).await;
        let probe = test_support::seed_session_with_permissions(&pool, &["sales.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);

        for uri in [
            "/api/customers",
            "/api/customers/ageing",
            &format!("/api/customers/{customer}"),
            &format!("/api/customers/{customer}/statement"),
            &format!("/api/customer-receipts?customer_id={customer}"),
        ] {
            let (st, v) = request_as(&app, "GET", uri, Some(&cookie), None).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{uri}: {v}");
            assert!(
                v["error"].as_str().unwrap_or_default().contains("customers.read"),
                "{uri} must name customers.read: {v}"
            );
        }

        // One receipt read: an unknown id still refuses the permission first.
        let (st, v) = request_as(&app, "GET", "/api/customer-receipts/999999", Some(&cookie), None).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("customers.read"),
            "{v}"
        );
    }
}
