use askama::Template;
use axum::{
    extract::{Form, Path, Query, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect},
    routing::{delete, get, post},
    Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::models::{PaymentMethod, TransactionKind};
use crate::repositories::AccountRepository;
use crate::routes::AppState;
use crate::services::finance_methods::PaymentMethodOption;

// ---------------------------------------------------------------------------
// Askama templates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    accounts: Vec<crate::models::AccountWithBalance>,
    total_balance: Decimal,
    allow_negative: bool,
    today: String,
    methods: Vec<PaymentMethod>,
    accounts_without_methods: AccountsWithoutMethods,
    nav_key: &'static str,
}

#[derive(Template)]
#[template(path = "account_detail.html")]
struct AccountDetailTemplate {
    account: crate::models::AccountWithBalance,
    transactions: Vec<crate::models::Transaction>,
    allow_negative: bool,
    methods: Vec<PaymentMethodOption>,
    has_methods: bool,
    nav_key: &'static str,
}

#[derive(Template)]
#[template(path = "partials/account_list.html")]
struct AccountListPartial {
    accounts: Vec<crate::models::AccountWithBalance>,
    total_balance: Decimal,
    accounts_without_methods: AccountsWithoutMethods,
}

#[derive(Template)]
#[template(path = "partials/transaction_list.html")]
struct TransactionListPartial {
    transactions: Vec<crate::models::Transaction>,
    account_id: i64,
}

#[derive(Template)]
#[template(path = "partials/account_options.html")]
struct AccountOptionsPartial {
    accounts: Vec<crate::models::AccountWithBalance>,
}

/// Accounts whose allowlist is empty. Exposes a template-friendly predicate so
/// the account list can flag the self-diagnosing warning without logic in HTML.
#[derive(Clone)]
struct AccountsWithoutMethods {
    ids: Vec<i64>,
}

impl AccountsWithoutMethods {
    fn missing(&self, account_id: &i64) -> bool {
        self.ids.contains(account_id)
    }
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

async fn missing_methods(state: &AppState) -> AppResult<AccountsWithoutMethods> {
    Ok(AccountsWithoutMethods {
        ids: state.payment_method_service.accounts_without_methods().await?,
    })
}

fn parse_method_id<'de, D>(value: &str) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    value.trim().parse::<i64>().map_err(|_| {
        <D::Error as serde::de::Error>::custom(format!("invalid payment method id: {value}"))
    })
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn dashboard(
    State(state): State<AppState>,
) -> Result<Html<String>, AppError> {
    let accounts = state.account_service.list_with_balances().await?;
    let total_balance = state.account_service.total_balance().await?;
    let methods = state.payment_method_service.list().await?;
    let accounts_without_methods = missing_methods(&state).await?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let tmpl = DashboardTemplate {
        accounts,
        total_balance,
        allow_negative: state.allow_negative,
        today,
        methods,
        accounts_without_methods,
        nav_key: "dashboard",
    };
    Ok(Html(tmpl.render().map_err(|e| AppError::Internal(e.to_string()))?))
}

async fn account_detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<axum::response::Response, AppError> {
    let detail = state.account_service.get_detail(id).await?;
    // find_with_balance for header
    let acc_with_balance = state.accounts_with_balance_lookup(id).await?;
    let methods = state.payment_method_service.catalog_for_account(id).await?;
    let has_methods = methods.iter().any(|m| m.allowed);
    let tmpl = AccountDetailTemplate {
        account: acc_with_balance,
        transactions: detail.transactions.clone(),
        allow_negative: state.allow_negative,
        methods,
        has_methods,
        nav_key: "accounts",
    };
    let html = tmpl.render().map_err(|e| AppError::Internal(e.to_string()))?;

    if is_htmx(&headers) {
        Ok(Html(html).into_response())
    } else {
        Ok(Html(html).into_response())
    }
}

// Need helper on AppState to get AccountWithBalance; implement via extension trait below
trait AccountLookup {
    async fn accounts_with_balance_lookup(
        &self,
        id: i64,
    ) -> AppResult<crate::models::AccountWithBalance>;
}
impl AccountLookup for AppState {
    async fn accounts_with_balance_lookup(
        &self,
        id: i64,
    ) -> AppResult<crate::models::AccountWithBalance> {
        self.account_service
            .accounts
            .find_with_balance(id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("account {id} not found")))
    }
}

// Forms
#[derive(Debug)]
pub struct CreateAccountForm {
    pub name: String,
    pub method_ids: Vec<i64>,
}

impl<'de> Deserialize<'de> for CreateAccountForm {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let pairs = Vec::<(String, String)>::deserialize(deserializer)?;
        let mut name = None;
        let mut method_ids = Vec::new();
        for (key, value) in pairs {
            match key.as_str() {
                "name" => name = Some(value),
                "method_ids" => method_ids.push(parse_method_id::<D>(&value)?),
                _ => {}
            }
        }
        let name =
            name.ok_or_else(|| <D::Error as serde::de::Error>::missing_field("name"))?;
        Ok(Self { name, method_ids })
    }
}

#[derive(Debug)]
pub struct UpdatePaymentMethodsForm {
    pub method_ids: Vec<i64>,
}

impl<'de> Deserialize<'de> for UpdatePaymentMethodsForm {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let pairs = Vec::<(String, String)>::deserialize(deserializer)?;
        let mut method_ids = Vec::new();
        for (key, value) in pairs {
            if key == "method_ids" {
                method_ids.push(parse_method_id::<D>(&value)?);
            }
        }
        Ok(Self { method_ids })
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateTransactionForm {
    pub account_id: i64,
    #[serde(rename = "type")]
    pub kind: String,
    pub amount: String,
    pub description: Option<String>,
    pub date: String,
}

async fn web_create_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateAccountForm>,
) -> Result<axum::response::Response, AppError> {
    // Explicit configuration: validate the allowlist before creating the
    // account so a rejected form never leaves a half-configured account behind.
    let method_ids = state
        .payment_method_service
        .validated_method_ids(&form.method_ids)
        .await?;
    let acc = state.account_service.create(&form.name).await?;
    state
        .payment_method_service
        .replace_allowed(acc.id, &method_ids)
        .await?;
    // If HTMX, return updated fragments
    if is_htmx(&headers) {
        let accounts = state.account_service.list_with_balances().await?;
        let total = state.account_service.total_balance().await?;
        let list_html = AccountListPartial {
            accounts: accounts.clone(),
            total_balance: total,
            accounts_without_methods: missing_methods(&state).await?,
        }
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
        let options_html = AccountOptionsPartial { accounts }
            .render()
            .map_err(|e| AppError::Internal(e.to_string()))?;

        // Return combined: account list + options via OOB swap
        // HTMX out-of-band swap: element with hx-swap-oob
        // We return list as main, and a hidden div that swaps options
        let combined = format!("{list_html}\n<div id=\"account-options\" hx-swap-oob=\"innerHTML\">{options_html}</div>");
        return Ok(Html(combined).into_response());
    }
    Ok(Redirect::to("/").into_response())
}

async fn web_update_payment_methods(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Form(form): Form<UpdatePaymentMethodsForm>,
) -> Result<axum::response::Response, AppError> {
    state.account_service.require_exists(id).await?;
    state
        .payment_method_service
        .replace_allowed(id, &form.method_ids)
        .await?;
    Ok(Redirect::to(&format!("/accounts/{id}")).into_response())
}

async fn web_create_transaction(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateTransactionForm>,
) -> Result<axum::response::Response, AppError> {
    let kind: TransactionKind = form
        .kind
        .parse()
        .map_err(|e: String| AppError::Validation(e))?;
    let amount: Decimal = form
        .amount
        .parse()
        .map_err(|_| AppError::Validation("invalid amount".into()))?;
    let date: NaiveDate = form
        .date
        .parse()
        .map_err(|_| AppError::Validation("invalid date (YYYY-MM-DD)".into()))?;

    state
        .transaction_service
        .create(form.account_id, kind, amount, form.description, date)
        .await?;

    if is_htmx(&headers) {
        // Return updated dashboard fragments
        let accounts = state.account_service.list_with_balances().await?;
        let total = state.account_service.total_balance().await?;
        let list_html = AccountListPartial {
            accounts,
            total_balance: total,
            accounts_without_methods: missing_methods(&state).await?,
        }
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
        // Also refresh transaction list for that account if on detail, via OOB?
        // For dashboard, we also clear form via JS; here just return list + trigger.
        // Tell HTMX to refresh: we use HX-Trigger header
        let mut resp = Html(list_html).into_response();
        resp.headers_mut()
            .insert("HX-Trigger", "transaction-created".parse().unwrap());
        return Ok(resp);
    }
    Ok(Redirect::to("/").into_response())
}

async fn web_delete_transaction(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<axum::response::Response, AppError> {
    state.transaction_service.delete(id).await?;
    // For HTMX, return empty 200 with trigger to refresh balances
    let mut resp = Html("".to_string()).into_response();
    resp.headers_mut()
        .insert("HX-Trigger", "transaction-deleted".parse().unwrap());
    // Also need to refresh account list: client will listen and GET /web/accounts
    Ok(resp)
}

// HTMX fragment: account list
async fn web_account_list(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let accounts = state.account_service.list_with_balances().await?;
    let total = state.account_service.total_balance().await?;
    let html = AccountListPartial {
        accounts,
        total_balance: total,
        accounts_without_methods: missing_methods(&state).await?,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

// HTMX fragment: transaction list for account
#[derive(Debug, Deserialize)]
pub struct TxListQuery {
    pub account_id: Option<i64>,
    pub from: Option<String>,
    pub to: Option<String>,
}

async fn web_transaction_list(
    State(state): State<AppState>,
    Query(q): Query<TxListQuery>,
) -> Result<Html<String>, AppError> {
    let filter = crate::models::TransactionFilter {
        account_id: q.account_id,
        from: q.from.as_deref().and_then(|s| s.parse().ok()),
        to: q.to.as_deref().and_then(|s| s.parse().ok()),
    };
    let txs = state.transaction_service.list(filter).await?;
    let account_id = q.account_id.unwrap_or(0);
    let html = TransactionListPartial {
        transactions: txs,
        account_id,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

// HTMX fragment: account options
async fn web_account_options(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let accounts = state.account_service.list_with_balances().await?;
    let html = AccountOptionsPartial { accounts }
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(dashboard))
        .route("/accounts/{id}", get(account_detail))
        .route(
            "/accounts/{id}/payment-methods",
            post(web_update_payment_methods),
        )
        .route("/web/accounts", get(web_account_list).post(web_create_account))
        .route("/web/account-options", get(web_account_options))
        .route(
            "/web/transactions",
            get(web_transaction_list).post(web_create_transaction),
        )
        .route("/web/transactions/{id}", delete(web_delete_transaction))
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

    const FORM: &str = "application/x-www-form-urlencoded";

    async fn test_state() -> AppState {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .unwrap()
            .create_if_missing(true)
            .foreign_keys(true)
            // Same posture as db::create_pool: customer triggers fire under REPLACE.
            .pragma("recursive_triggers", "1");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        AppState::new(pool, false, true)
    }

    async fn send(
        app: Router,
        method: &str,
        uri: &str,
        content_type: Option<&str>,
        body: String,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(ct) = content_type {
            builder = builder.header("content-type", ct);
        }
        let req = builder.body(Body::from(body)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    async fn post_json(app: &Router, uri: &str, body: serde_json::Value) -> (StatusCode, String) {
        send(
            app.clone(),
            "POST",
            uri,
            Some("application/json"),
            body.to_string(),
        )
        .await
    }

    async fn method_id(pool: &sqlx::SqlitePool, name: &str) -> i64 {
        let row: (i64,) = sqlx::query_as("SELECT id FROM payment_methods WHERE name = ?")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap();
        row.0
    }

    async fn account_id(pool: &sqlx::SqlitePool, name: &str) -> i64 {
        let row: (i64,) = sqlx::query_as("SELECT id FROM accounts WHERE name = ?")
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap();
        row.0
    }

    async fn web_create_account(
        app: &Router,
        name: &str,
        method_ids: &[i64],
    ) -> (StatusCode, String) {
        let mut parts: Vec<String> = method_ids
            .iter()
            .map(|id| format!("method_ids={id}"))
            .collect();
        parts.push(format!("name={name}"));
        send(app.clone(), "POST", "/web/accounts", Some(FORM), parts.join("&")).await
    }

    async fn seed_product(app: &Router, sku: &str) -> i64 {
        let (status, body) = post_json(
            app,
            "/api/products",
            serde_json::json!({
                "sku": sku, "name": format!("prod {sku}"), "kind": "Product",
                "unit": "un", "sale_price": "10", "cost_price": "5",
                "track_stock": true, "min_stock": "5", "max_stock": "50"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "seed product {sku}: {body}");
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_i64()
            .unwrap()
    }

    async fn seed_stock(app: &Router, product_id: i64) {
        let (status, body) = post_json(
            app,
            "/api/stock-movements",
            serde_json::json!({
                "product_id": product_id, "qty": "10", "type": "In",
                "reason": "Initial", "date": "2024-05-01"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "seed stock: {body}");
    }

    async fn seed_credit_sale(app: &Router, pool: &sqlx::SqlitePool) -> i64 {
        let (customer_id,): (i64,) = sqlx::query_as(
            "INSERT INTO customers (name) VALUES ('Regression Buyer') RETURNING id",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        let (status, body) = post_json(
            app,
            "/api/sales",
            serde_json::json!({
                "customer_id": customer_id, "payment_type": "Credit",
                "sale_date": "2024-05-02", "due_date": "2024-06-01"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "seed sale: {body}");
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["sale"]["id"]
            .as_i64()
            .unwrap()
    }

    async fn pay(
        app: &Router,
        sale_id: i64,
        account: i64,
        method: i64,
        amount: &str,
    ) -> (StatusCode, String) {
        post_json(
            app,
            &format!("/api/sales/{sale_id}/payments"),
            serde_json::json!({
                "account_id": account, "method_id": method,
                "amount": amount, "date": "2024-05-10"
            }),
        )
        .await
    }

    #[tokio::test]
    async fn web_create_account_with_methods_then_record_payment_succeeds() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);

        let cash = method_id(&pool, "Cash").await;
        // Original bug: the web form created accounts with an empty
        // account_payment_methods allowlist, so every payment was rejected.
        let (status, body) = web_create_account(&app, "Wallet", &[cash]).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "web create: {status} {body}");
        let acc = account_id(&pool, "Wallet").await;

        let pid = seed_product(&app, "REG-E2E").await;
        seed_stock(&app, pid).await;
        let sale = seed_credit_sale(&app, &pool).await;
        let (status, body) = post_json(
            &app,
            &format!("/api/sales/{sale}/lines"),
            serde_json::json!({ "product_id": pid, "qty": "1" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "add line: {body}");
        let (status, body) = post_json(
            &app,
            &format!("/api/sales/{sale}/confirm"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "confirm: {body}");

        let (status, body) = pay(&app, sale, acc, cash, "10").await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "payment must be accepted after web creation with the allowlist: {body}"
        );
        let payments: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sale_payments WHERE sale_id = ?")
            .bind(sale)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(payments.0, 1, "payment row persisted");
    }

    #[tokio::test]
    async fn payment_rejected_with_actionable_message_when_account_has_no_methods() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);

        // REST-created accounts start with no allowlist (web form requires a tick).
        let (status, body) = post_json(
            &app,
            "/api/accounts",
            serde_json::json!({ "name": "Unconfigured" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "create account: {body}");
        let acc: i64 = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_i64()
            .unwrap();
        let cash = method_id(&pool, "Cash").await;

        let pid = seed_product(&app, "NO-ALLOW").await;
        seed_stock(&app, pid).await;
        let sale = seed_credit_sale(&app, &pool).await;
        let (status, body) = post_json(
            &app,
            &format!("/api/sales/{sale}/lines"),
            serde_json::json!({ "product_id": pid, "qty": "1" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "add line: {body}");
        let (status, body) = post_json(
            &app,
            &format!("/api/sales/{sale}/confirm"),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "confirm: {body}");

        let (status, body) = pay(&app, sale, acc, cash, "10").await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "must stay a 400 Validation: {body}"
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let msg = v["error"].as_str().unwrap_or_default();
        assert!(
            msg.contains("configure the account's payment methods"),
            "message must tell the user what to do, got {msg}"
        );
        let payments: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sale_payments WHERE sale_id = ?")
            .bind(sale)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(payments.0, 0, "no side effects on rejection");
    }

    #[tokio::test]
    async fn web_create_account_without_ticked_methods_is_rejected() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);

        let (status, body) = web_create_account(&app, "NoMethods", &[]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.contains("at least one"), "clear message: {body}");
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM accounts WHERE name = 'NoMethods'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count.0, 0, "no account may be created without methods");
    }

    #[tokio::test]
    async fn web_update_payment_methods_replaces_set() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let cash = method_id(&pool, "Cash").await;
        let transfer = method_id(&pool, "Transfer").await;

        let (status, body) = web_create_account(&app, "Switcher", &[cash]).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        let acc = account_id(&pool, "Switcher").await;

        let (status, body) = send(
            app.clone(),
            "POST",
            &format!("/accounts/{acc}/payment-methods"),
            Some(FORM),
            format!("method_ids={transfer}"),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "save: {status} {body}");

        let (status, body) = send(
            app.clone(),
            "GET",
            &format!("/api/accounts/{acc}/payment-methods"),
            None,
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let allowed: Vec<&str> = v["methods"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["allowed"] == true)
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert_eq!(allowed, vec!["Transfer"], "Cash must be removed: {body}");

        // The detail page reflects the new set and clears the warning.
        let (status, page) = send(
            app.clone(),
            "GET",
            &format!("/accounts/{acc}"),
            None,
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            page.contains(&format!("value=\"{transfer}\" checked")),
            "Transfer must render checked"
        );
        assert!(
            !page.contains(&format!("value=\"{cash}\" checked")),
            "Cash must render unchecked"
        );
        assert!(!page.contains("No payment methods configured"));
    }

    #[tokio::test]
    async fn account_detail_flags_missing_payment_methods() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let cash = method_id(&pool, "Cash").await;

        let (status, body) = post_json(
            &app,
            "/api/accounts",
            serde_json::json!({ "name": "BareDetail" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let bare = account_id(&pool, "BareDetail").await;
        let (status, page) = send(
            app.clone(),
            "GET",
            &format!("/accounts/{bare}"),
            None,
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            page.contains("No payment methods configured"),
            "warning must be visible: {page}"
        );
        assert!(
            page.contains("data-payment-methods-warning"),
            "warning needs a machine-checkable marker"
        );

        let (status, _) = web_create_account(&app, "ConfiguredDetail", &[cash]).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let configured = account_id(&pool, "ConfiguredDetail").await;
        let (status, page) = send(
            app.clone(),
            "GET",
            &format!("/accounts/{configured}"),
            None,
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !page.contains("No payment methods configured"),
            "configured account must not be flagged: {page}"
        );
    }

    #[tokio::test]
    async fn account_list_flags_accounts_without_methods() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);
        let cash = method_id(&pool, "Cash").await;

        let (status, _) = post_json(
            &app,
            "/api/accounts",
            serde_json::json!({ "name": "ListBare" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let bare = account_id(&pool, "ListBare").await;

        let (status, list) = send(app.clone(), "GET", "/web/accounts", None, String::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            list.contains("No payment methods configured"),
            "list must flag the broken account: {list}"
        );

        let (status, body) = send(
            app.clone(),
            "PUT",
            &format!("/api/accounts/{bare}/payment-methods"),
            Some("application/json"),
            serde_json::json!({ "method_ids": [cash] }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");

        let (status, list) = send(app.clone(), "GET", "/web/accounts", None, String::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !list.contains("No payment methods configured"),
            "flag must clear once configured: {list}"
        );
    }
}
