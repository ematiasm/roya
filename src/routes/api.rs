use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json, Router,
    routing::{get, put},
};
use serde::Deserialize;

use crate::error::AppResult;
use crate::models::{CreateAccountRequest, CreateTransactionRequest, TransactionFilter, UpdateTransactionRequest};
use crate::routes::AppState;
use crate::security::authz::{
    FinanceMethodsManage, FinanceRead, FinanceWrite, Require,
};

// S5 enforcement mapping (finance JSON API): reads → `finance.read`,
// transaction mutations → `finance.write`, and the account/method allowlist
// surface → `finance.methods.manage` (its seeded description is "Administrar
// cuentas y medios de pago": creating an account and assigning its payment
// methods is account management, not transaction recording).

async fn list_accounts(
    State(state): State<AppState>,
    _: Require<FinanceRead>,
) -> AppResult<Json<serde_json::Value>> {
    let accounts = state.account_service.list_with_balances().await?;
    let total = state.account_service.total_balance().await?;
    Ok(Json(serde_json::json!({ "accounts": accounts, "total_balance": total })))
}

async fn create_account(
    State(state): State<AppState>,
    _: Require<FinanceMethodsManage>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Json(payload): Json<CreateAccountRequest>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let acc = state.account_service.create(principal.user_id, &payload.name).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(acc))))
}

async fn get_account(
    State(state): State<AppState>,
    _: Require<FinanceRead>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let detail = state.account_service.get_detail(id).await?;
    Ok(Json(serde_json::json!(detail)))
}

/// Every payment method with its owning account, so clients can resolve a name
/// to the row one account owns without going through an account's catalog.
async fn list_payment_methods(
    State(state): State<AppState>,
    _: Require<FinanceRead>,
) -> AppResult<Json<serde_json::Value>> {
    let methods = state.payment_method_service.list().await?;
    Ok(Json(serde_json::json!({ "methods": methods })))
}

/// Body for `PUT /api/accounts/{id}/payment-methods`. The list replaces the
/// account's method set: ids owned by another account are a 400 (never stolen);
/// an empty list unassigns everything (the UI warns on method-less accounts).
#[derive(Debug, Deserialize)]
struct UpdateAccountPaymentMethodsRequest {
    #[serde(default)]
    method_ids: Vec<i64>,
}

/// The account's own methods (ownership implies usability).
async fn get_account_payment_methods(
    State(state): State<AppState>,
    _: Require<FinanceMethodsManage>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    state.account_service.require_exists(id).await?;
    let methods = state.payment_method_service.catalog_for_account(id).await?;
    Ok(Json(methods_json(id, methods)))
}

/// Replace the account's method set with `method_ids` (unknown ids => 404,
/// foreign-owned ids => 400). Returns the updated set.
async fn put_account_payment_methods(
    State(state): State<AppState>,
    _: Require<FinanceMethodsManage>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateAccountPaymentMethodsRequest>,
) -> AppResult<Json<serde_json::Value>> {
    state.account_service.require_exists(id).await?;
    let methods = state
        .payment_method_service
        .replace_account_methods(principal.user_id, id, &payload.method_ids)
        .await?;
    Ok(Json(methods_json(id, methods)))
}

/// Shape shared by GET and PUT: the account plus its owned methods.
fn methods_json(
    account_id: i64,
    methods: Vec<crate::models::PaymentMethod>,
) -> serde_json::Value {
    let method_ids: Vec<i64> = methods.iter().map(|m| m.id).collect();
    serde_json::json!({
        "account_id": account_id,
        "method_ids": method_ids,
        "methods": methods,
    })
}

async fn list_transactions(
    State(state): State<AppState>,
    _: Require<FinanceRead>,
    Query(filter): Query<TransactionFilter>,
) -> AppResult<Json<serde_json::Value>> {
    let txs = state.transaction_service.list(filter).await?;
    Ok(Json(serde_json::json!({ "transactions": txs })))
}

async fn create_transaction(
    State(state): State<AppState>,
    _: Require<FinanceWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Json(payload): Json<CreateTransactionRequest>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let tx = state
        .transaction_service
        .create_with_reference(
            principal.user_id,
            payload.account_id,
            payload.kind,
            payload.amount,
            payload.description,
            payload.reference,
            payload.date,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(tx))))
}

async fn update_transaction(
    State(state): State<AppState>,
    _: Require<FinanceWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateTransactionRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let tx = state
        .transaction_service
        .update(principal.user_id, id, payload.kind, payload.amount, payload.description, payload.date)
        .await?;
    Ok(Json(serde_json::json!(tx)))
}

async fn delete_transaction(
    State(state): State<AppState>,
    _: Require<FinanceWrite>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    state.transaction_service.delete(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/accounts", get(list_accounts).post(create_account))
        .route("/api/accounts/{id}", get(get_account))
        .route("/api/payment-methods", get(list_payment_methods))
        .route(
            "/api/accounts/{id}/payment-methods",
            get(get_account_payment_methods).put(put_account_payment_methods),
        )
        .route("/api/transactions", get(list_transactions).post(create_transaction))
        .route(
            "/api/transactions/{id}",
            put(update_transaction).delete(delete_transaction),
        )
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
        Router,
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

    async fn send(
        app: Router,
        method: &str,
        uri: &str,
        content_type: Option<&str>,
        body: String,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = test_support::with_cookie(Request::builder().method(method).uri(uri));
        if let Some(ct) = content_type {
            builder = builder.header("content-type", ct);
        }
        let req = builder.body(Body::from(body)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    /// Same as [`send`], but with an explicit cookie: `None` means the truly
    /// anonymous request the deny-by-default tests need (the shared TEST_COOKIE
    /// belongs to the full-permission principal, never to a refusal probe).
    async fn send_as(
        app: Router,
        method: &str,
        uri: &str,
        cookie: Option<&str>,
        content_type: Option<&str>,
        body: String,
    ) -> (StatusCode, serde_json::Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        if let Some(ct) = content_type {
            builder = builder.header("content-type", ct);
        }
        let req = builder.body(Body::from(body)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    async fn method_id(pool: &sqlx::SqlitePool, name: &str) -> i64 {
        let row: (i64,) = sqlx::query_as("SELECT id FROM payment_methods WHERE name = ?")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap();
        row.0
    }

    async fn create_account(app: &Router, name: &str) -> i64 {
        let (status, v) = send(
            app.clone(),
            "POST",
            "/api/accounts",
            Some("application/json"),
            serde_json::json!({ "name": name }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "create account: {v}");
        v["id"].as_i64().unwrap()
    }

    async fn get_methods(app: &Router, account_id: i64) -> (StatusCode, serde_json::Value) {
        send(
            app.clone(),
            "GET",
            &format!("/api/accounts/{account_id}/payment-methods"),
            None,
            String::new(),
        )
        .await
    }

    async fn put_methods(
        app: &Router,
        account_id: i64,
        ids: &[i64],
    ) -> (StatusCode, serde_json::Value) {
        send(
            app.clone(),
            "PUT",
            &format!("/api/accounts/{account_id}/payment-methods"),
            Some("application/json"),
            serde_json::json!({ "method_ids": ids }).to_string(),
        )
        .await
    }

    fn owned_names(v: &serde_json::Value) -> Vec<String> {
        let mut names: Vec<String> = v["methods"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap().to_string())
            .collect();
        names.sort();
        names
    }

    // -- S5 enforcement (AC10): the permission gate on the real handlers ------

    /// A principal holding ONLY `finance.read` can read the finance surfaces
    /// and is refused every mutation, including the account/method allowlist
    /// (`finance.methods.manage`), in the JSON shape `/api/*` callers read.
    #[tokio::test]
    async fn ac10_a_finance_read_only_principal_reads_and_is_refused_the_writes() {
        let state = test_state().await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["finance.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        // The read the probe is allowed.
        let (st, _) = send_as(
            app.clone(),
            "GET",
            "/api/accounts",
            Some(&cookie),
            None,
            String::new(),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "finance.read must open the reads");

        // Transaction mutation: the JSON refusal names the gate.
        let (st, v) = send_as(
            app.clone(),
            "POST",
            "/api/transactions",
            Some(&cookie),
            Some("application/json"),
            serde_json::json!({
                "account_id": 1, "type": "Income", "amount": "10",
                "description": "denied", "date": "2024-01-15"
            })
            .to_string(),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("finance.write"),
            "the refusal must name finance.write: {v}"
        );

        // Account creation is account management, not transaction recording:
        // the allowlist permission gates it.
        let (st, v) = send_as(
            app.clone(),
            "POST",
            "/api/accounts",
            Some(&cookie),
            Some("application/json"),
            serde_json::json!({"name": "Denied Account"}).to_string(),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("finance.methods.manage"),
            "the refusal must name finance.methods.manage: {v}"
        );

        // The payment-method allowlist, both directions of the same endpoint.
        let (st, v) = send_as(
            app.clone(),
            "GET",
            "/api/accounts/1/payment-methods",
            Some(&cookie),
            None,
            String::new(),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("finance.methods.manage"),
            "{v}"
        );
        let (st, v) = send_as(
            app.clone(),
            "PUT",
            "/api/accounts/1/payment-methods",
            Some(&cookie),
            Some("application/json"),
            serde_json::json!({"method_ids": [1]}).to_string(),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("finance.methods.manage"),
            "{v}"
        );
    }

    /// The refusal writes nothing: the read-only principal's refused
    /// transaction leaves the ledger exactly where it was.
    #[tokio::test]
    async fn ac10_a_finance_refusal_writes_nothing() {
        let state = test_state().await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["finance.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        // Stage: a real account with one real transaction, written by the
        // full-permission principal.
        let (st, _) = send_as(
            app.clone(),
            "POST",
            "/api/accounts",
            Some(test_support::TEST_COOKIE),
            Some("application/json"),
            serde_json::json!({"name": "Refusal Proof"}).to_string(),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let (st, _) = send_as(
            app.clone(),
            "POST",
            "/api/transactions",
            Some(test_support::TEST_COOKIE),
            Some("application/json"),
            serde_json::json!({
                "account_id": 1, "type": "Income", "amount": "50",
                "description": "real", "date": "2024-01-14"
            })
            .to_string(),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED);
        let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions")
            .fetch_one(&state.pool)
            .await
            .unwrap();

        let (st, v) = send_as(
            app.clone(),
            "POST",
            "/api/transactions",
            Some(&cookie),
            Some("application/json"),
            serde_json::json!({
                "account_id": 1, "type": "Income", "amount": "1",
                "description": "must not exist", "date": "2024-01-16"
            })
            .to_string(),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");

        let after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions")
            .fetch_one(&state.pool)
            .await
            .unwrap();
        assert_eq!(after, before, "a refused request must write nothing");
    }

    /// A principal holding the permission gets its normal status on the very
    /// same endpoints: the gate is about the SET, not the route.
    #[tokio::test]
    async fn ac10_the_holding_principal_gets_the_normal_answer() {
        let state = test_state().await;
        let token = test_support::seed_session_with_permissions(
            &state.pool,
            &["finance.read", "finance.write", "finance.methods.manage"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&token);
        let app = crate::routes::router(state.clone());

        let (st, v) = send_as(
            app.clone(),
            "POST",
            "/api/accounts",
            Some(&cookie),
            Some("application/json"),
            serde_json::json!({"name": "Holder Cash"}).to_string(),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        let account_id = v["id"].as_i64().unwrap();

        let (st, v) = send_as(
            app.clone(),
            "POST",
            "/api/transactions",
            Some(&cookie),
            Some("application/json"),
            serde_json::json!({
                "account_id": account_id, "type": "Income", "amount": "25",
                "description": "held", "date": "2024-01-15"
            })
            .to_string(),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");

        let cash = method_id(&state.pool, "Cash").await;
        let (st, _) = send_as(
            app.clone(),
            "PUT",
            &format!("/api/accounts/{account_id}/payment-methods"),
            Some(&cookie),
            Some("application/json"),
            serde_json::json!({"method_ids": [cash]}).to_string(),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
    }

    /// The gate runs FIRST: an anonymous request keeps the deny-by-default
    /// refusal (401 JSON), never the permission refusal.
    #[tokio::test]
    async fn an_anonymous_request_still_gets_the_login_gate_not_the_permission_refusal() {
        let app = crate::routes::router(test_state().await);
        let (st, v) = send_as(app.clone(), "GET", "/api/accounts", None, None, String::new()).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
        assert_eq!(v["error"].as_str(), Some("unauthorized"), "{v}");
    }

    #[tokio::test]
    async fn get_account_payment_methods_lists_owned_methods() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let acc = create_account(&app, "ApiCatalog").await;
        let cash = method_id(&pool, "Cash").await;

        // A fresh account owns nothing (ownership, not a full catalog).
        let (status, v) = get_methods(&app, acc).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert!(v["methods"].as_array().unwrap().is_empty(), "{v}");
        assert!(v["method_ids"].as_array().unwrap().is_empty(), "{v}");
        assert!(v["methods"].as_array().unwrap().iter().all(|m| {
            m.get("id").is_some()
                && m.get("name").is_some()
                && m.get("is_active").is_some()
                && m.get("account_id").is_some()
        }));

        let (status, v) = put_methods(&app, acc, &[cash]).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(owned_names(&v), vec!["Cash"]);
        assert_eq!(
            v["method_ids"].as_array().unwrap(),
            &vec![serde_json::json!(cash)],
            "owned ids are exposed explicitly: {v}"
        );

        let (status, v) = get_methods(&app, acc).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(owned_names(&v), vec!["Cash"]);

        let (status, _) = get_methods(&app, 999_999).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "unknown account must 404");
    }

    #[tokio::test]
    async fn put_account_payment_methods_replaces_set_instead_of_merging() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let acc = create_account(&app, "ApiReplace").await;
        let cash = method_id(&pool, "Cash").await;
        let transfer = method_id(&pool, "Transfer").await;

        let (status, v) = put_methods(&app, acc, &[cash, transfer]).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(owned_names(&v), vec!["Cash", "Transfer"]);

        let (status, v) = put_methods(&app, acc, &[transfer]).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(
            owned_names(&v),
            vec!["Transfer"],
            "Cash must be unassigned, not kept: {v}"
        );

        let (status, v) = get_methods(&app, acc).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(owned_names(&v), vec!["Transfer"]);
    }

    #[tokio::test]
    async fn put_account_payment_methods_rejects_unknown_method_id() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let acc = create_account(&app, "ApiUnknown").await;
        let cash = method_id(&pool, "Cash").await;
        let transfer = method_id(&pool, "Transfer").await;
        let (status, _) = put_methods(&app, acc, &[cash]).await;
        assert_eq!(status, StatusCode::OK);

        let (status, v) = put_methods(&app, acc, &[transfer, 999_999]).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "unknown method must 404: {v}");

        let (status, v) = get_methods(&app, acc).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            owned_names(&v),
            vec!["Cash"],
            "a rejected PUT must not mutate the set: {v}"
        );
    }

    #[tokio::test]
    async fn put_account_payment_methods_rejects_foreign_owned_method_id() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let a = create_account(&app, "ApiOwner").await;
        let b = create_account(&app, "ApiThief").await;
        let cash = method_id(&pool, "Cash").await;
        let (status, _) = put_methods(&app, a, &[cash]).await;
        assert_eq!(status, StatusCode::OK);

        // Stealing is a 400 and changes nothing on either side.
        let (status, v) = put_methods(&app, b, &[cash]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "foreign method must 400: {v}");
        assert!(
            v["error"].as_str().unwrap_or_default().contains("belongs to account"),
            "actionable message: {v}"
        );
        let (status, v) = get_methods(&app, a).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(owned_names(&v), vec!["Cash"]);
        let (status, v) = get_methods(&app, b).await;
        assert_eq!(status, StatusCode::OK);
        assert!(owned_names(&v).is_empty());
    }

    #[tokio::test]
    async fn put_account_payment_methods_accepts_empty_list_and_unassigns() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let acc = create_account(&app, "ApiEmpty").await;
        let cash = method_id(&pool, "Cash").await;
        let (status, _) = put_methods(&app, acc, &[cash]).await;
        assert_eq!(status, StatusCode::OK);

        let (status, v) = put_methods(&app, acc, &[]).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert!(owned_names(&v).is_empty(), "empty unassigns everything: {v}");

        let (status, v) = get_methods(&app, acc).await;
        assert_eq!(status, StatusCode::OK);
        assert!(owned_names(&v).is_empty());
    }

    // -- transaction reference (money traceability) -----------------------------

    #[tokio::test]
    async fn transaction_api_reference_is_null_for_manual_transactions() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let acc = create_account(&app, "ApiManualRef").await;

        let (status, v) = send(
            app.clone(),
            "POST",
            "/api/transactions",
            Some("application/json"),
            serde_json::json!({
                "account_id": acc,
                "type": "Income",
                "amount": "100.50",
                "description": "Salary",
                "date": "2024-01-15"
            })
            .to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        assert!(v.get("reference").is_some(), "reference must be exposed: {v}");
        assert!(v["reference"].is_null(), "manual transaction has no reference: {v}");

        let stored: (Option<String>,) =
            sqlx::query_as("SELECT reference FROM transactions WHERE id = ?")
                .bind(v["id"].as_i64().unwrap())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored.0, None);

        // The list response exposes the same field.
        let (status, v) = send(
            app.clone(),
            "GET",
            &format!("/api/transactions?account_id={acc}"),
            None,
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert!(v["transactions"][0]["reference"].is_null(), "{v}");
    }

    #[tokio::test]
    async fn transaction_api_persists_explicit_reference() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let acc = create_account(&app, "ApiExplicitRef").await;

        let (status, v) = send(
            app.clone(),
            "POST",
            "/api/transactions",
            Some("application/json"),
            serde_json::json!({
                "account_id": acc,
                "type": "Income",
                "amount": "20",
                "description": "opaque note",
                "reference": "OPAQUE-REF-1",
                "date": "2024-01-16"
            })
            .to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        assert_eq!(v["reference"], "OPAQUE-REF-1", "{v}");

        let stored: (Option<String>,) =
            sqlx::query_as("SELECT reference FROM transactions WHERE id = ?")
                .bind(v["id"].as_i64().unwrap())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored.0.as_deref(), Some("OPAQUE-REF-1"));
    }

    #[tokio::test]
    async fn transaction_api_delete_manual_returns_204_and_removes_row() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let acc = create_account(&app, "ApiDelete").await;

        // Fund the account so the Expense create passes the balance guard.
        let (status, v) = send(
            app.clone(),
            "POST",
            "/api/transactions",
            Some("application/json"),
            serde_json::json!({
                "account_id": acc,
                "type": "Income",
                "amount": "100",
                "description": "float",
                "date": "2024-01-14"
            })
            .to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{v}");

        let (status, v) = send(
            app.clone(),
            "POST",
            "/api/transactions",
            Some("application/json"),
            serde_json::json!({
                "account_id": acc,
                "type": "Expense",
                "amount": "12.50",
                "description": "manual",
                "date": "2024-01-15"
            })
            .to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        let tx_id = v["id"].as_i64().unwrap();

        // Manual delete path keeps its previous status code.
        let (status, body) = send(
            app.clone(),
            "DELETE",
            &format!("/api/transactions/{tx_id}"),
            None,
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

        let stored: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions WHERE id = ?")
            .bind(tx_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored.0, 0, "manual delete must remove the row");
    }

    #[tokio::test]
    async fn transaction_api_delete_linked_returns_409_and_keeps_row() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let acc = create_account(&app, "ApiDeleteLinked").await;

        // Fund the account so the Expense create passes the balance guard.
        let (status, v) = send(
            app.clone(),
            "POST",
            "/api/transactions",
            Some("application/json"),
            serde_json::json!({
                "account_id": acc,
                "type": "Income",
                "amount": "100",
                "description": "float",
                "date": "2024-01-14"
            })
            .to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{v}");

        let (status, v) = send(
            app.clone(),
            "POST",
            "/api/transactions",
            Some("application/json"),
            serde_json::json!({
                "account_id": acc,
                "type": "Expense",
                "amount": "12.50",
                "description": "sale payment",
                "date": "2024-01-15"
            })
            .to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        let tx_id = v["id"].as_i64().unwrap();

        // Synthetic sale payment pointing at the money row (RESTRICT).
        // `customer_id` is NOT NULL by design: resolve the seeded walk-in
        // instead of hardcoding its id.
        let sale_id: (i64,) = sqlx::query_as(
            "INSERT INTO sales (status, payment_type, customer_id, customer_name, sale_date) \
             VALUES ('Confirmed', 'Cash', \
                     (SELECT id FROM customers WHERE is_walkin = 1), 'fixture', '2024-01-15') \
             RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sale_payments \
             (sale_id, account_id, method_id, amount, date, transaction_id) \
             VALUES (?, ?, 1, '12.50', '2024-01-15', ?)",
        )
        .bind(sale_id.0)
        .bind(acc)
        .bind(tx_id)
        .execute(&pool)
        .await
        .unwrap();

        let (status, body) = send(
            app.clone(),
            "DELETE",
            &format!("/api/transactions/{tx_id}"),
            None,
            String::new(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "linked delete must be 409, got {body}"
        );
        let msg = body["error"].as_str().unwrap_or_default();
        assert!(
            msg.contains("cancel the document"),
            "message must be actionable, got: {msg}"
        );

        let stored: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM transactions WHERE id = ?")
            .bind(tx_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stored.0, 1, "linked row must survive");
        let linked: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM sale_payments WHERE transaction_id = ?")
                .bind(tx_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(linked.0, 1, "payment must stay linked");
    }
}
