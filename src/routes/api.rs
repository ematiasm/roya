use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, put},
    Json, Router,
};
use serde::Deserialize;

use crate::error::AppResult;
use crate::models::{
    Account, AccountDetail, AccountMethodsResponse, AccountsResponse, CreateAccountRequest,
    CreateTransactionRequest, MethodsResponse, PaymentMethod, Transaction, TransactionFilter,
    TransactionsResponse, UpdateTransactionRequest,
};
use crate::routes::AppState;
use crate::security::authz::{FinanceMethodsManage, FinanceRead, FinanceWrite, Require};

// S5 enforcement mapping (finance JSON API): reads → `finance.read`,
// transaction mutations → `finance.write`, and the account/method allowlist
// surface → `finance.methods.manage` (its seeded description is "Administrar
// cuentas y medios de pago": creating an account and assigning its payment
// methods is account management, not transaction recording).

async fn list_accounts(
    State(state): State<AppState>,
    _: Require<FinanceRead>,
) -> AppResult<Json<AccountsResponse>> {
    // Tolerant, like every other list in this change: an account whose balance
    // refuses travels as a row with the rule in it, and the set total refuses with
    // it, so the document still lists every OTHER account instead of collapsing
    // into one error.
    let (accounts, total) = state.account_service.list_with_balances_and_total().await?;
    Ok(Json(AccountsResponse {
        accounts,
        total_balance: total,
    }))
}

async fn create_account(
    State(state): State<AppState>,
    _: Require<FinanceMethodsManage>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Json(payload): Json<CreateAccountRequest>,
) -> AppResult<(StatusCode, Json<Account>)> {
    // `Caja` arrives with `Cash`, `Banco` with its card/tranfer names: the
    // defaults are part of what creating that account MEANS, so the two surfaces
    // that can create one agree instead of the web form being the only one that
    // wires them.
    let acc = state
        .account_service
        .create_with_default_methods(principal.user_id, &payload.name)
        .await?;
    // Ticked ids join on top, through the same door the web form uses: a name
    // owned elsewhere is duplicated into this account rather than stolen.
    for method_id in &payload.method_ids {
        state
            .payment_method_service
            .assign_or_duplicate(principal.user_id, acc.id, *method_id)
            .await?;
    }
    Ok((StatusCode::CREATED, Json(acc)))
}

async fn get_account(
    State(state): State<AppState>,
    _: Require<FinanceRead>,
    Path(id): Path<i64>,
) -> AppResult<Json<AccountDetail>> {
    let detail = state.account_service.get_detail(id).await?;
    Ok(Json(detail))
}

/// Every payment method with its owning account, so clients can resolve a name
/// to the row one account owns without going through an account's catalog.
async fn list_payment_methods(
    State(state): State<AppState>,
    _: Require<FinanceRead>,
) -> AppResult<Json<MethodsResponse>> {
    let methods = state.payment_method_service.list().await?;
    Ok(Json(MethodsResponse { methods }))
}

/// Body for `PUT /api/accounts/{id}/payment-methods`. The list replaces the
/// account's SELECTABLE method set: ids owned by another account are a 400 (never
/// stolen), and a method that was owned here and is no longer listed is
/// DEACTIVATED rather than unassigned — migration 45 made an unowned method
/// unrepresentable, and the owner is the historical fact a refund reads back. An
/// empty list therefore leaves the account with nothing to collect through, which
/// the UI warns about.
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
) -> AppResult<Json<AccountMethodsResponse>> {
    state.account_service.require_exists(id).await?;
    let methods = state.payment_method_service.catalog_for_account(id).await?;
    Ok(Json(account_methods(id, methods)))
}

/// Replace the account's method set with `method_ids` (unknown ids => 404,
/// foreign-owned ids => 400). Returns the updated set.
async fn put_account_payment_methods(
    State(state): State<AppState>,
    _: Require<FinanceMethodsManage>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateAccountPaymentMethodsRequest>,
) -> AppResult<Json<AccountMethodsResponse>> {
    state.account_service.require_exists(id).await?;
    let methods = state
        .payment_method_service
        .replace_account_methods(principal.user_id, id, &payload.method_ids)
        .await?;
    Ok(Json(account_methods(id, methods)))
}

/// Shape shared by GET and PUT: the account plus its owned methods.
fn account_methods(account_id: i64, methods: Vec<PaymentMethod>) -> AccountMethodsResponse {
    let method_ids: Vec<i64> = methods.iter().map(|m| m.id).collect();
    AccountMethodsResponse {
        account_id,
        method_ids,
        methods,
    }
}

async fn list_transactions(
    State(state): State<AppState>,
    _: Require<FinanceRead>,
    Query(filter): Query<TransactionFilter>,
) -> AppResult<Json<TransactionsResponse>> {
    let txs = state.transaction_service.list(filter).await?;
    Ok(Json(TransactionsResponse { transactions: txs }))
}

async fn create_transaction(
    State(state): State<AppState>,
    _: Require<FinanceWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Json(payload): Json<CreateTransactionRequest>,
) -> AppResult<(StatusCode, Json<Transaction>)> {
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
    Ok((StatusCode::CREATED, Json(tx)))
}

async fn update_transaction(
    State(state): State<AppState>,
    _: Require<FinanceWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateTransactionRequest>,
) -> AppResult<Json<Transaction>> {
    let tx = state
        .transaction_service
        .update(
            principal.user_id,
            id,
            payload.kind,
            payload.amount,
            payload.description,
            payload.date,
        )
        .await?;
    Ok(Json(tx))
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
        .route(
            "/api/transactions",
            get(list_transactions).post(create_transaction),
        )
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

    use crate::models::PriceRefusal;
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

    /// The method an account owns under that name.
    ///
    /// Migration 45 made ownership NOT NULL and seeded `Cash` on `Caja`, so a
    /// bare name lookup is ambiguous the moment two accounts have one. Every
    /// caller here means "the method THIS account owns", because that is what the
    /// pair guard and the editor both require.
    async fn method_id_in_account(pool: &sqlx::SqlitePool, account_id: i64, name: &str) -> i64 {
        let row: (i64,) =
            sqlx::query_as("SELECT id FROM payment_methods WHERE account_id = ? AND name = ?")
                .bind(account_id)
                .bind(name)
                .fetch_one(pool)
                .await
                .unwrap();
        row.0
    }

    /// The method THIS account owns under `name`, created through the same
    /// `PUT` the editor drives — which duplicates a name owned elsewhere instead
    /// of stealing it, and is the only way to end up with a second row of a name.
    async fn own_method(app: &Router, pool: &sqlx::SqlitePool, account: i64, name: &str) -> i64 {
        // The id does not exist yet, so the PUT cannot name it. Duplicate through
        // the repo the same way `assign_or_duplicate` does, then read it back:
        // this is fixture setup, and the guarantee under test is the READ and the
        // refusal, not the duplication itself.
        let actor = test_support::audit_actor_id(pool).await.unwrap();
        sqlx::query(
            "INSERT INTO payment_methods (name, account_id, is_active, created_by) \
             VALUES (?, ?, 1, ?)",
        )
        .bind(name)
        .bind(account)
        .bind(actor)
        .execute(pool)
        .await
        .unwrap();
        let _ = app;
        method_id_in_account(pool, account, name).await
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

    /// The names this account can actually collect through (active only).
    fn active_names(v: &serde_json::Value) -> Vec<String> {
        let mut names: Vec<String> = v["methods"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["is_active"] == serde_json::json!(true))
            .map(|m| m["name"].as_str().unwrap().to_string())
            .collect();
        names.sort();
        names
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

        // The probe's own account owns its own Cash; the seeded row belongs to the
        // migration's `Caja` and `PUT` would refuse it as foreign.
        let cash = own_method(&app, &state.pool, account_id, "Cash").await;
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
        let (st, v) = send_as(
            app.clone(),
            "GET",
            "/api/accounts",
            None,
            None,
            String::new(),
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
        assert_eq!(v["error"].as_str(), Some("unauthorized"), "{v}");
    }

    #[tokio::test]
    async fn get_account_payment_methods_lists_owned_methods() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let acc = create_account(&app, "ApiCatalog").await;

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

        // Minting a foreign name into this account is the editor's business, not
        // the fixture's: `PUT` refuses a method another account owns, so the
        // account gets its own row and the PUT names THAT.
        let cash = own_method(&app, &pool, acc, "Cash").await;
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
        let cash = own_method(&app, &pool, acc, "Cash").await;
        let transfer = own_method(&app, &pool, acc, "Transfer").await;

        let (status, v) = put_methods(&app, acc, &[cash, transfer]).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(owned_names(&v), vec!["Cash", "Transfer"]);

        let (status, v) = put_methods(&app, acc, &[transfer]).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(
            owned_names(&v),
            vec!["Cash", "Transfer"],
            "the unticked method stays in the catalog, deactivated: {v}"
        );
        let cash_row = v["methods"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == serde_json::json!(cash))
            .expect("the deactivated method is still reported");
        assert_eq!(
            cash_row["is_active"],
            serde_json::json!(false),
            "migration 45: unticking deactivates, it does not unassign: {v}"
        );
        assert_eq!(
            cash_row["account_id"],
            serde_json::json!(acc),
            "and the owner is kept, because the stored account is history: {v}"
        );

        let (status, v) = get_methods(&app, acc).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(owned_names(&v), vec!["Cash", "Transfer"]);
    }

    #[tokio::test]
    async fn put_account_payment_methods_rejects_unknown_method_id() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let acc = create_account(&app, "ApiUnknown").await;
        let cash = own_method(&app, &pool, acc, "Cash").await;
        let transfer = own_method(&app, &pool, acc, "Transfer").await;
        let (status, _) = put_methods(&app, acc, &[cash]).await;
        assert_eq!(status, StatusCode::OK);

        let (status, v) = put_methods(&app, acc, &[transfer, 999_999]).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "unknown method must 404: {v}"
        );

        let (status, v) = get_methods(&app, acc).await;
        assert_eq!(status, StatusCode::OK);
        // What must not mutate is the SELECTABLE set. The catalog also reports the
        // deactivated rows, and `Transfer` was already deactivated by the first PUT
        // above, so the claim is stated over `is_active` rather than over names.
        assert_eq!(
            active_names(&v),
            vec!["Cash"],
            "a rejected PUT must not change what the account can collect through: {v}"
        );
        assert_eq!(
            owned_names(&v),
            vec!["Cash", "Transfer"],
            "and it must not unassign the row it refused to keep: {v}"
        );
    }

    #[tokio::test]
    async fn put_account_payment_methods_rejects_foreign_owned_method_id() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let a = create_account(&app, "ApiOwner").await;
        let b = create_account(&app, "ApiThief").await;
        let cash = own_method(&app, &pool, a, "Cash").await;
        let (status, _) = put_methods(&app, a, &[cash]).await;
        assert_eq!(status, StatusCode::OK);

        // Stealing is a 400 and changes nothing on either side.
        let (status, v) = put_methods(&app, b, &[cash]).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "foreign method must 400: {v}"
        );
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .contains("belongs to account"),
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
    async fn put_account_payment_methods_accepts_empty_list_and_deactivates() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let acc = create_account(&app, "ApiEmpty").await;
        let cash = own_method(&app, &pool, acc, "Cash").await;
        let (status, _) = put_methods(&app, acc, &[cash]).await;
        assert_eq!(status, StatusCode::OK);

        // An empty list leaves nothing SELECTABLE — the state the warning is
        // about — while the row stays owned and inactive, so it can be ticked
        // again instead of being re-created under a name that already exists.
        let (status, v) = put_methods(&app, acc, &[]).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert!(
            v["methods"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["is_active"] == serde_json::json!(false)),
            "empty deactivates everything: {v}"
        );
        assert!(
            v["methods"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["account_id"] == serde_json::json!(acc)),
            "and nothing is unassigned: {v}"
        );

        let (status, v) = get_methods(&app, acc).await;
        assert_eq!(status, StatusCode::OK);
        assert!(v["methods"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| { m["is_active"] == serde_json::json!(false) }));
    }

    // -- transaction reference (money traceability) -----------------------------

    /// The sweep's FOURTH fold, and the money twin of the stock level: an account
    /// balance is the sum of that account's transactions, every one of them
    /// written from a request's amount, and `validate_amount` refuses only
    /// `amount <= 0`. Two incomes of `4e28` carry on their own, are `8e28`
    /// together, and the accounts list — the finance home page — folds them raw.
    #[tokio::test]
    async fn an_account_balance_whose_transactions_cannot_be_added_up_is_not_a_panic() {
        let state = test_state().await;
        let app = crate::routes::router(state);
        let account = create_account(&app, "Sweep Balance").await;
        let income = |date: &'static str| {
            let app = app.clone();
            async move {
                send(
                    app,
                    "POST",
                    "/api/transactions",
                    Some("application/json"),
                    serde_json::json!({
                        "account_id": account,
                        "type": "Income",
                        "amount": "40000000000000000000000000000",
                        "description": "sweep",
                        "date": date,
                    })
                    .to_string(),
                )
                .await
            }
        };

        let (status, v) = income("2024-05-01").await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        // The second is the sum that does not carry, refused BEFORE the write.
        let (status, v) = income("2024-05-02").await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a balance the addition cannot carry is a refusal, not a panic: {v}"
        );
        assert!(
            v.to_string().contains("too large to compute"),
            "in the rule's own words: {v}"
        );

        // The accounts list still answers, with the balance that carried.
        let (status, v) = send(app.clone(), "GET", "/api/accounts", None, String::new()).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let balance = v["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"].as_i64() == Some(account))
            .map(|a| a["balance"].as_str().unwrap_or_default().to_string())
            .unwrap_or_default();
        assert_eq!(balance, "40000000000000000000000000000", "{v}");
    }

    /// B1: an UPDATE with an empty body still recomputes the projected balance,
    /// and that projection was a raw `current - orig_signed + new_signed` — two
    /// operators, no bound, on a route every `finance.write` caller can reach.
    ///
    /// The state is built entirely through the application's OWN checked writes
    /// with `allow_negative = false`, so the test cannot pass on a state the
    /// application refuses to create: income `5e28`, expense `4.9e28`, income
    /// `5e28`, where every prefix carries and the balance is `5.1e28`. Reverting
    /// the expense to income then projects `5.1e28 + 4.9e28 = 1e29`, which no
    /// `Decimal` carries.
    #[tokio::test]
    async fn a_transaction_update_whose_projection_cannot_be_carried_is_a_refusal() {
        let state = test_state().await; // allow_negative = false
        let app = crate::routes::router(state);
        let account = create_account(&app, "Update Projection").await;
        let post = |kind: &'static str, amount: &'static str, date: &'static str| {
            let app = app.clone();
            async move {
                let (status, v) = send(
                    app,
                    "POST",
                    "/api/transactions",
                    Some("application/json"),
                    serde_json::json!({
                        "account_id": account,
                        "type": kind,
                        "amount": amount,
                        "description": "checked write",
                        "date": date,
                    })
                    .to_string(),
                )
                .await;
                (status, v)
            }
        };

        // Three ordinary writes, every one of them accepted by the checked
        // pre-check — so the state is one the application itself produces.
        let (status, v) = post("Income", "50000000000000000000000000000", "2024-05-01").await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        let (status, expense) =
            post("Expense", "49000000000000000000000000000", "2024-05-02").await;
        assert_eq!(status, StatusCode::CREATED, "{v}");
        let expense_id = expense["id"].as_i64().unwrap();
        let (status, v) = post("Income", "50000000000000000000000000000", "2024-05-03").await;
        assert_eq!(status, StatusCode::CREATED, "{v}");

        // The empty-body update: nothing about the transaction changes, and the
        // projection still has to be computed.
        let (status, v) = send(
            app.clone(),
            "PUT",
            &format!("/api/transactions/{expense_id}"),
            Some("application/json"),
            serde_json::json!({ "type": "Income" }).to_string(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a projection that cannot be carried is a refusal, not a panic: {v}"
        );
        assert!(
            v.to_string().contains("too large to compute"),
            "in the rule's own words: {v}"
        );

        // And nothing was written: the transaction is still the expense it was,
        // and the balance is still the one the three writes produced.
        // The list is the shape the API offers for a single transaction (there is
        // no per-id read), and it answers either way.
        let (status, v) = send(
            app.clone(),
            "GET",
            &format!("/api/transactions?account_id={account}"),
            None,
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let row = v["transactions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"].as_i64() == Some(expense_id))
            .unwrap_or_else(|| panic!("the expense is still in the list: {v}"));
        assert_eq!(row["kind"], serde_json::json!("Expense"), "{v}");
        assert_eq!(
            row["amount"],
            serde_json::json!("49000000000000000000000000000"),
            "with the amount the refused update would have changed: {v}"
        );
        let (status, v) = send(app.clone(), "GET", "/api/accounts", None, String::new()).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        let balance = v["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"].as_i64() == Some(account))
            .map(|a| a["balance"].as_str().unwrap_or_default().to_string())
            .unwrap_or_default();
        assert_eq!(balance, "51000000000000000000000000000", "{v}");
    }

    /// The wire convention F4 established, on the accounts list: an ordinary
    /// response is byte-identical to before, and a refused member carries the
    /// rule instead of taking the document down with a 400.
    ///
    /// The pair of `4e28` incomes is stored straight through SQL, because the
    /// transaction write refuses the second one by design — which is exactly the
    /// state an account can be found in, and the state this endpoint has to
    /// render.
    #[tokio::test]
    async fn the_accounts_api_renders_a_refused_balance_on_the_wire() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let refused = create_account(&app, "Api Refused").await;
        let ordinary = create_account(&app, "Api Ordinary").await;
        let actor = test_support::audit_actor_id(&state.pool).await.unwrap();
        for (account, amount) in [
            (refused, "40000000000000000000000000000"),
            (refused, "40000000000000000000000000000"),
            (ordinary, "1250"),
        ] {
            sqlx::query(
                "INSERT INTO transactions (account_id, kind, amount, description, date, created_by) \
                 VALUES (?, 'Income', ?, 'sweep', '2024-05-01', ?)",
            )
            .bind(account)
            .bind(amount)
            .bind(actor)
            .execute(&state.pool)
            .await
            .unwrap();
        }

        let (status, v) = send(app.clone(), "GET", "/api/accounts", None, String::new()).await;
        assert_eq!(status, StatusCode::OK, "the list answers: {v}");
        let row = |id: i64| {
            v["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["id"].as_i64() == Some(id))
                .unwrap_or_else(|| panic!("account {id} missing from the list: {v}"))
                .clone()
        };
        // The refused account: the rule travels with the figure's place.
        let refused_row = row(refused);
        assert_eq!(
            refused_row["balance"]["refused"],
            serde_json::json!(PriceRefusal::AggregateTooLarge.as_str()),
            "{v}"
        );
        assert!(
            refused_row.get("cached_balance").is_none(),
            "and NO stale cached figure stands in for it: {v}"
        );
        // The ordinary account: byte-identical to the shape before this change.
        let ordinary_row = row(ordinary);
        assert_eq!(ordinary_row["balance"], serde_json::json!("1250"), "{v}");
        assert!(
            ordinary_row.get("cached_balance").is_some(),
            "and the cached field is still there, exactly as before: {v}"
        );
        // The SET total sums the refused member, so it refuses — but it refuses as
        // a figure with a reason, not as an error that emptied the document.
        assert_eq!(
            v["total_balance"]["refused"],
            serde_json::json!(PriceRefusal::AggregateTooLarge.as_str()),
            "{v}"
        );

        // The account's own detail is a list of one and renders the same way.
        let (status, v) = send(
            app.clone(),
            "GET",
            &format!("/api/accounts/{refused}"),
            None,
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the detail answers: {v}");
        assert_eq!(
            v["balance"]["refused"],
            serde_json::json!(PriceRefusal::AggregateTooLarge.as_str()),
            "{v}"
        );
    }

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
        assert!(
            v.get("reference").is_some(),
            "reference must be exposed: {v}"
        );
        assert!(
            v["reference"].is_null(),
            "manual transaction has no reference: {v}"
        );

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
            "INSERT INTO sales (status, payment_type, customer_id, customer_name, sale_date, created_by) \
             VALUES ('Confirmed', 'Cash', \
                     (SELECT id FROM customers WHERE is_walkin = 1), 'fixture', '2024-01-15', ?) \
             RETURNING id",
        )
        .bind(test_support::audit_actor_id(&pool).await.unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
        // Migration 44 guards the (account, method) pair on the payment row,
        // so the fixture pairs the money's account with a method it OWNS (the
        // seeded methods are unassigned on a fresh database).
        let method_id: i64 = match sqlx::query_scalar(
            "SELECT id FROM payment_methods WHERE name = 'fixture cash' AND account_id = ?",
        )
        .bind(acc)
        .fetch_optional(&pool)
        .await
        .unwrap()
        {
            Some(id) => id,
            None => sqlx::query_scalar(
                "INSERT INTO payment_methods (name, account_id, created_by) \
                 VALUES ('fixture cash', ?, ?) RETURNING id",
            )
            .bind(acc)
            .bind(test_support::audit_actor_id(&pool).await.unwrap())
            .fetch_one(&pool)
            .await
            .unwrap(),
        };
        sqlx::query(
            "INSERT INTO sale_payments \
             (sale_id, account_id, method_id, amount, date, transaction_id, created_by) \
             VALUES (?, ?, ?, '12.50', '2024-01-15', ?, ?)",
        )
        .bind(sale_id.0)
        .bind(acc)
        .bind(method_id)
        .bind(tx_id)
        .bind(test_support::audit_actor_id(&pool).await.unwrap())
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
