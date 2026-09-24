use askama::Template;
use axum::{
    extract::{Extension, Form, Path, Query, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect},
    routing::{delete, get, post},
    Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::localization::LocalizationContext;
use crate::models::{PaymentMethod, TransactionKind};
use crate::repositories::AccountRepository;
use crate::routes::AppState;
use crate::security::authz::{
    DashboardRead, FinanceMethodsManage, FinanceRead, FinanceWrite, Nav, Require,
};

// S5 enforcement mapping (dashboard + finance HTML/HTMX): the dashboard reads
// `dashboard.read`; finance reads `finance.read`; transaction mutations
// `finance.write`; the account/method allowlist and account creation
// `finance.methods.manage` (seeded description: "Administrar cuentas y medios
// de pago" — the detail page itself is a finance READ and stays open to
// read-only operators; the handler, not the markup, is the enforcement).

// ---------------------------------------------------------------------------
// Askama templates
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardTemplate {
    accounts: Vec<crate::models::AccountWithBalance>,
    total_balance: Decimal,
    localization: LocalizationContext,
    allow_negative: bool,
    today: String,
    methods: Vec<PaymentMethod>,
    accounts_without_methods: AccountsWithoutMethods,
    /// Whether the acting principal may see the accounts block: the block's
    /// data is a finance read (`finance.read`, the gate `/web/accounts` and
    /// the account API carry), and the `accounts` nav entry names the block —
    /// so the dashboard renders it conditionally on the same code, the way
    /// the suggestions block does in purchases_web.rs. The rest of the page
    /// stays the dashboard's own screen (`dashboard.read`); the account
    /// balances are a separable card, so nothing is left implicit here.
    show_accounts: bool,
    nav_key: &'static str,
    /// The sidebar's nav view: the entries this principal may read (S7 part 2).
    nav: Nav,
}

/// One history row of the account detail view with its audit actors resolved
/// to display names (M5 Phase B): the finance department returns ids; the
/// wiring layer resolves them (see `audit_actor_names` in routes/mod.rs), so
/// the interface shows a name and never an id.
struct TransactionRow {
    tx: crate::models::Transaction,
    /// Display name of the user that created the movement; `None` only when
    /// the id resolves to nothing (a concurrent deactivation).
    created_by_name: Option<String>,
    /// Display name of the last editor, when the movement was edited at all.
    updated_by_name: Option<String>,
}

#[derive(Template)]
#[template(path = "account_detail.html")]
struct AccountDetailTemplate {
    account: crate::models::AccountWithBalance,
    transactions: Vec<TransactionRow>,
    localization: LocalizationContext,
    /// Display name of the account's creator ("Registrado por"). Every
    /// account has one (`created_by` is NOT NULL); it renders even when the
    /// actor is the migration's sentinel, whose display name says exactly
    /// what happened.
    account_created_by_name: Option<String>,
    /// Display name of the last editor ("Actualizado por"), only rendered
    /// when the account has been edited.
    account_updated_by_name: Option<String>,
    allow_negative: bool,
    methods: Vec<PaymentMethod>,
    unassigned: Vec<PaymentMethod>,
    has_methods: bool,
    nav_key: &'static str,
    /// The sidebar's nav view: the entries this principal may read (S7 part 2).
    nav: Nav,
}

#[derive(Template)]
#[template(path = "partials/account_list.html")]
struct AccountListPartial {
    accounts: Vec<crate::models::AccountWithBalance>,
    total_balance: Decimal,
    accounts_without_methods: AccountsWithoutMethods,
    localization: LocalizationContext,
}

#[derive(Template)]
#[template(path = "partials/transaction_list.html")]
struct TransactionListPartial {
    transactions: Vec<crate::models::Transaction>,
    account_id: i64,
    localization: LocalizationContext,
}

#[derive(Template)]
#[template(path = "partials/account_options.html")]
struct AccountOptionsPartial {
    accounts: Vec<crate::models::AccountWithBalance>,
    localization: LocalizationContext,
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
    _: Require<DashboardRead>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
) -> Result<Html<String>, AppError> {
    let accounts = state.account_service.list_with_balances().await?;
    let total_balance = state.account_service.total_balance().await?;
    let methods = state.payment_method_service.list().await?;
    let accounts_without_methods = missing_methods(&state).await?;
    let today = localization.today_iso();
    let tmpl = DashboardTemplate {
        accounts,
        total_balance,
        localization,
        allow_negative: state.allow_negative,
        today,
        methods,
        accounts_without_methods,
        show_accounts: principal.has_permission::<FinanceRead>(),
        nav_key: "dashboard",
        nav: Nav::for_principal(&principal),
    };
    Ok(Html(tmpl.render().map_err(|e| AppError::Internal(e.to_string()))?))
}

async fn account_detail(
    State(state): State<AppState>,
    _: Require<FinanceRead>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Result<axum::response::Response, AppError> {
    let detail = state.account_service.get_detail(id).await?;
    // find_with_balance for header
    let acc_with_balance = state.accounts_with_balance_lookup(id).await?;
    let methods = state.payment_method_service.catalog_for_account(id).await?;
    let unassigned = state.payment_method_service.unassigned().await?;
    let has_methods = !methods.is_empty();
    // The audit actors are resolved HERE, in the wiring layer, because a
    // department may not read identity tables (AC20) and the view must show a
    // name, never an id. One statement covers the account header and every
    // history row.
    let mut actor_ids: Vec<i64> = detail.transactions.iter().map(|tx| tx.created_by).collect();
    actor_ids.extend(detail.transactions.iter().filter_map(|tx| tx.updated_by));
    actor_ids.push(detail.created_by);
    actor_ids.extend(detail.updated_by);
    let names = crate::routes::audit_actor_names(&state.pool, &actor_ids).await?;
    let name_for = |id: i64| names.get(&id).cloned();
    let transactions = detail
        .transactions
        .iter()
        .map(|tx| TransactionRow {
            created_by_name: name_for(tx.created_by),
            updated_by_name: tx.updated_by.and_then(name_for),
            tx: tx.clone(),
        })
        .collect();
    let tmpl = AccountDetailTemplate {
        account_created_by_name: name_for(detail.created_by),
        account_updated_by_name: detail.updated_by.and_then(name_for),
        account: acc_with_balance,
        transactions,
        localization,
        allow_negative: state.allow_negative,
        methods,
        unassigned,
        has_methods,
        nav_key: "accounts",
        nav: Nav::for_principal(&principal),
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
    _: Require<FinanceMethodsManage>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<CreateAccountForm>,
) -> Result<axum::response::Response, AppError> {
    // Ticked methods join the new account: unassigned ones are assigned, ones
    // owned elsewhere are duplicated by name (never stolen). No ticks means a
    // method-less account, which the list flags with a warning. Every write
    // carries the acting user (M5 Phase B): the account and the methods it
    // gains record the same request's actor.
    let actor = principal.user_id;
    let acc = state.account_service.create(actor, &form.name).await?;
    for method_id in &form.method_ids {
        state
            .payment_method_service
            .assign_or_duplicate(actor, acc.id, *method_id)
            .await?;
    }
    // If HTMX, return updated fragments
    if is_htmx(&headers) {
        let accounts = state.account_service.list_with_balances().await?;
        let total = state.account_service.total_balance().await?;
        let list_html = AccountListPartial {
            accounts: accounts.clone(),
            total_balance: total,
            accounts_without_methods: missing_methods(&state).await?,
            localization: localization.clone(),
        }
        .render()
        .map_err(|e| AppError::Internal(e.to_string()))?;
        let options_html = AccountOptionsPartial {
            accounts,
            localization: localization.clone(),
        }
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
    _: Require<FinanceMethodsManage>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Path(id): Path<i64>,
    Form(form): Form<UpdatePaymentMethodsForm>,
) -> Result<axum::response::Response, AppError> {
    state.account_service.require_exists(id).await?;
    state
        .payment_method_service
        .replace_account_methods(principal.user_id, id, &form.method_ids)
        .await?;
    Ok(Redirect::to(&format!("/accounts/{id}")).into_response())
}

async fn web_create_transaction(
    State(state): State<AppState>,
    _: Require<FinanceWrite>,
    principal: axum::Extension<crate::security::authz::Principal>,
    Extension(localization): Extension<LocalizationContext>,
    headers: HeaderMap,
    Form(form): Form<CreateTransactionForm>,
) -> Result<axum::response::Response, AppError> {
    let kind: TransactionKind = form
        .kind
        .parse()
        .map_err(|e: String| AppError::Validation(e))?;
    let amount = localization
        .parse_decimal(&form.amount)
        .map_err(|_| AppError::Validation("invalid amount".into()))?;
    let date: NaiveDate = form
        .date
        .parse()
        .map_err(|_| AppError::Validation("invalid date (YYYY-MM-DD)".into()))?;

    state
        .transaction_service
        .create(principal.user_id, form.account_id, kind, amount, form.description, date)
        .await?;

    if is_htmx(&headers) {
        // Return updated dashboard fragments
        let accounts = state.account_service.list_with_balances().await?;
        let total = state.account_service.total_balance().await?;
        let list_html = AccountListPartial {
            accounts,
            total_balance: total,
            accounts_without_methods: missing_methods(&state).await?,
            localization: localization.clone(),
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
    _: Require<FinanceWrite>,
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
async fn web_account_list(
    State(state): State<AppState>,
    _: Require<FinanceRead>,
    Extension(localization): Extension<LocalizationContext>,
) -> Result<Html<String>, AppError> {
    let accounts = state.account_service.list_with_balances().await?;
    let total = state.account_service.total_balance().await?;
    let html = AccountListPartial {
        accounts,
        total_balance: total,
        accounts_without_methods: missing_methods(&state).await?,
        localization,
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
    _: Require<FinanceRead>,
    Extension(localization): Extension<LocalizationContext>,
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
        localization,
    }
    .render()
    .map_err(|e| AppError::Internal(e.to_string()))?;
    Ok(Html(html))
}

// HTMX fragment: account options
async fn web_account_options(
    State(state): State<AppState>,
    _: Require<FinanceRead>,
    Extension(localization): Extension<LocalizationContext>,
) -> Result<Html<String>, AppError> {
    let accounts = state.account_service.list_with_balances().await?;
    let html = AccountOptionsPartial {
        accounts,
        localization,
    }
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
    use crate::security::test_support;

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
        // S1b part 1: seed the fixed test session every request will authenticate with.
        test_support::seed_session(&pool).await.unwrap();
        AppState::new(pool, false, true)
    }

    // -- S5 enforcement (AC10): the permission gate on the real handlers ------

    /// Same as [`send`], but with an explicit cookie: `None` means the truly
    /// anonymous request the deny-by-default tests need (the shared
    /// TEST_COOKIE belongs to the full-permission principal).
    async fn send_as(
        app: Router,
        method: &str,
        uri: &str,
        cookie: Option<&str>,
        content_type: Option<&str>,
        extra_headers: &[(&str, &str)],
        body: String,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        if let Some(ct) = content_type {
            builder = builder.header("content-type", ct);
        }
        for (name, value) in extra_headers {
            builder = builder.header(*name, *value);
        }
        let req = builder.body(Body::from(body)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// A principal holding ONLY `finance.read` is refused the finance web
    /// mutations in the shape each caller reads: an HTMX form gets the JSON
    /// the global notice box renders, a plain browser post gets the full-page
    /// refusal card.
    #[tokio::test]
    async fn ac10_a_finance_read_only_principal_is_refused_the_web_mutations_in_both_shapes() {
        let state = test_state().await;
        let probe = test_support::seed_session_with_permissions(&state.pool, &["finance.read"])
            .await
            .unwrap();
        let cookie = test_support::cookie_for(&probe);
        let app = crate::routes::router(state.clone());

        // Creating an account over HTMX: JSON naming the allowlist gate.
        let (status, body) = send_as(
            app.clone(),
            "POST",
            "/web/accounts",
            Some(&cookie),
            Some(FORM),
            &[("HX-Request", "true")],
            "name=Denied+HTMX".to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("finance.methods.manage"),
            "the HTMX refusal must name finance.methods.manage: {json}"
        );

        // The same create as a plain browser post: the HTML refusal card.
        let (status, body) = send_as(
            app.clone(),
            "POST",
            "/web/accounts",
            Some(&cookie),
            Some(FORM),
            &[],
            "name=Denied+Page".to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body:.400}");
        assert!(
            body.contains("Acción no permitida"),
            "the refusal must speak Spanish: {body:.400}"
        );
        assert!(
            body.contains("finance.methods.manage"),
            "the refusal must name the missing permission: {body:.400}"
        );

        // Recording a transaction over HTMX: its own gate, finance.write.
        let (status, body) = send_as(
            app.clone(),
            "POST",
            "/web/transactions",
            Some(&cookie),
            Some(FORM),
            &[("HX-Request", "true")],
            "account_id=1&type=Income&amount=10&date=2024-01-15".to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("finance.write"),
            "the HTMX refusal must name finance.write: {json}"
        );
    }

    /// A principal holding the permission gets its normal status: the
    /// dashboard opens with `dashboard.read` and the transaction form answers
    /// its HTMX fragment with `finance.write`.
    #[tokio::test]
    async fn ac10_the_holding_principal_gets_the_normal_answer() {
        let state = test_state().await;
        let token = test_support::seed_session_with_permissions(
            &state.pool,
            &["dashboard.read", "finance.read", "finance.write"],
        )
        .await
        .unwrap();
        let cookie = test_support::cookie_for(&token);
        let app = crate::routes::router(state.clone());

        let (status, page) = send_as(app.clone(), "GET", "/", Some(&cookie), None, &[], String::new()).await;
        assert_eq!(status, StatusCode::OK, "{page:.400}");

        // Stage an account as the shared full-permission principal, then the
        // holder records the transaction.
        let (status, _) = send(
            app.clone(),
            "POST",
            "/api/accounts",
            Some("application/json"),
            serde_json::json!({"name": "Holder Web"}).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let account = account_id(&state.pool, "Holder Web").await;
        let (status, body) = send_as(
            app.clone(),
            "POST",
            "/web/transactions",
            Some(&cookie),
            Some(FORM),
            &[("HX-Request", "true")],
            format!("account_id={account}&type=Income&amount=30&date=2024-01-15"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body:.400}");
    }

    /// The gate runs FIRST: an anonymous request keeps the deny-by-default
    /// login redirect, never the permission refusal.
    #[tokio::test]
    async fn an_anonymous_request_still_gets_the_login_gate_not_the_permission_refusal() {
        let app = crate::routes::router(test_state().await);
        let (status, _) = send_as(app.clone(), "GET", "/", None, None, &[], String::new()).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let (status, _) = send_as(
            app.clone(),
            "GET",
            "/accounts/1",
            None,
            None,
            &[],
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER, "every page, same gate");
    }

    /// The dashboard's own gate, pinned by the correction round: a principal
    /// WITHOUT `dashboard.read` gets the full-page refusal naming it, and the
    /// principal that holds it answers 200. (An inventory-only probe, so the
    /// refusal is this page's gate and not a broken fixture.)
    #[tokio::test]
    async fn the_dashboard_gate_refuses_a_principal_without_it_and_opens_with_it() {
        let state = test_state().await;
        let reader = test_support::seed_session_with_permissions(&state.pool, &["inventory.read"])
            .await
            .unwrap();
        let holder = test_support::seed_session_with_permissions(&state.pool, &["dashboard.read"])
            .await
            .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, body) = send_as(
            app.clone(),
            "GET",
            "/",
            Some(&test_support::cookie_for(&reader)),
            None,
            &[],
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body:.400}");
        assert!(
            body.contains("Acción no permitida"),
            "the refusal must speak Spanish: {body:.400}"
        );
        assert!(
            body.contains("dashboard.read"),
            "the refusal must name the missing permission: {body:.400}"
        );

        let (status, page) = send_as(
            app,
            "GET",
            "/",
            Some(&test_support::cookie_for(&holder)),
            None,
            &[],
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page:.400}");
    }

    // -- S7 part 2 (AC21): the sidebar tells the truth -------------------------

    /// A principal holding only `dashboard.read` + `inventory.read` sees the
    /// dashboard and products entries (plus the password entry every signed-in
    /// operator keeps) and NOTHING else — in particular no entry whose page
    /// would refuse it, and no empty group heading. Both directions: what is
    /// readable is shown, what is not is absent from the markup entirely.
    #[tokio::test]
    async fn ac21_a_limited_principal_sees_exactly_the_entries_it_may_read() {
        let state = test_state().await;
        let probe = test_support::seed_session_with_permissions(
            &state.pool,
            &["dashboard.read", "inventory.read"],
        )
        .await
        .unwrap();
        let app = crate::routes::router(state.clone());

        let (status, html) = send_as(
            app.clone(),
            "GET",
            "/",
            Some(&test_support::cookie_for(&probe)),
            None,
            &[],
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        for key in ["dashboard", "products", "password"] {
            assert!(
                html.contains(&format!("data-nav=\"{key}\"")),
                "the readable entry {key} must render: {html:.600}"
            );
        }
        for key in [
            "sales",
            "purchases",
            "suppliers",
            "customers",
            "accounts",
            "users",
            "roles",
        ] {
            assert!(
                !html.contains(&format!("data-nav=\"{key}\"")),
                "the entry {key} must be hidden from this principal: {html:.600}"
            );
        }
        // No empty group headings: operation and catalogue have entries, cash
        // (finance.read) and the identity rows do not.
        assert!(html.contains("data-nav-group=\"operation\""), "{html:.600}");
        assert!(html.contains("data-nav-group=\"catalogue\""), "{html:.600}");
        assert!(!html.contains("data-nav-group=\"cash\""), "{html:.600}");
        assert!(!html.contains("Usuarios"), "{html:.600}");
        assert!(!html.contains("Finanzas"), "{html:.600}");
    }

    /// The full-permission principal (the shared fixture holds the whole
    /// catalog) sees every entry the sidebar declares — the administrator
    /// never loses a screen.
    #[tokio::test]
    async fn ac21_the_full_permission_principal_sees_every_entry() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let (status, html) = send_as(app.clone(), "GET", "/", Some(test_support::TEST_COOKIE), None, &[], String::new()).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        for key in [
            "dashboard",
            "sales",
            "purchases",
            "products",
            "suppliers",
            "customers",
            "accounts",
            "users",
            "roles",
            "password",
        ] {
            assert_eq!(
                count_key(&html, &format!("data-nav=\"{key}\"")),
                1,
                "the full-permission principal must see {key} exactly once: {html:.600}"
            );
        }
    }

    /// The signed-in user's name renders next to the logout control: the
    /// display name and the username, from the request's principal.
    #[tokio::test]
    async fn ac21_the_sidebar_shows_the_signed_in_user_next_to_logout() {
        let state = test_state().await;
        let app = crate::routes::router(state.clone());
        let (status, html) = send_as(app.clone(), "GET", "/", Some(test_support::TEST_COOKIE), None, &[], String::new()).await;
        assert_eq!(status, StatusCode::OK, "{html:.400}");
        assert!(
            html.contains("data-sidebar-user"),
            "the sidebar must carry the signed-in user block: {html:.600}"
        );
        assert!(
            html.contains("Test Admin") && html.contains("test-admin"),
            "the display name and the username must render: {html:.600}"
        );
        assert!(
            html.contains("action=\"/logout\""),
            "the logout control stays: {html:.600}"
        );
    }

    /// The one two-code entry: `accounts` declares `dashboard.read` (the gate
    /// of the `/` route its href opens) AND `finance.read` (the data owner of
    /// the accounts block its label names). A principal holding only ONE of
    /// the two codes must NOT see the entry: the finance-only principal is
    /// refused the page outright (its 403 shell keeps only what it may read),
    /// and the dashboard-only principal opens the page but the accounts block
    /// hides with it. Holding BOTH, the entry and the block render. The raw
    /// fragments are printed so a human can read the actual markup.
    #[tokio::test]
    async fn ac21_the_two_code_accounts_entry_shows_only_to_principals_holding_both() {
        let state = test_state().await;
        let finance_only = test_support::seed_session_with_permissions(&state.pool, &["finance.read"])
            .await
            .unwrap();
        let dashboard_only =
            test_support::seed_session_with_permissions(&state.pool, &["dashboard.read"])
                .await
                .unwrap();
        let both = test_support::seed_session_with_permissions(
            &state.pool,
            &["dashboard.read", "finance.read"],
        )
        .await
        .unwrap();
        let app = crate::routes::router(state.clone());

        // One code (finance.read): the href's route refuses the principal, and
        // the refusal's shell must not promise the entry either.
        let (status, page) = send_as(
            app.clone(),
            "GET",
            "/",
            Some(&test_support::cookie_for(&finance_only)),
            None,
            &[],
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{page:.400}");
        assert!(
            !page.contains("data-nav=\"accounts\""),
            "a finance.read-only principal must not see the accounts entry: {page:.600}"
        );
        println!(
            "[fragment] finance.read only -> GET / answers 403; the refusal shell carries \
             no accounts anchor (data-nav=\"accounts\" absent)"
        );

        // The other code (dashboard.read): the page opens, the accounts block
        // hides (web.rs renders it conditionally on finance.read), the entry
        // hides with it.
        let (status, page) = send_as(
            app.clone(),
            "GET",
            "/",
            Some(&test_support::cookie_for(&dashboard_only)),
            None,
            &[],
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page:.400}");
        assert!(
            !page.contains("data-nav=\"accounts\""),
            "a dashboard.read-only principal must not see the accounts entry: {page:.600}"
        );
        assert!(
            !page.contains("id=\"accounts\""),
            "a dashboard.read-only principal must not see the accounts block: {page:.600}"
        );
        println!(
            "[fragment] dashboard.read only -> GET / answers 200; sidebar anchor and accounts \
             card (id=\"accounts\") both absent"
        );

        // Both codes: the entry renders, and with it the block it names.
        let (status, page) = send_as(
            app,
            "GET",
            "/",
            Some(&test_support::cookie_for(&both)),
            None,
            &[],
            String::new(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{page:.400}");
        assert!(
            page.contains("data-nav=\"accounts\""),
            "a principal holding both codes must see the accounts entry: {page:.600}"
        );
        assert!(
            page.contains("id=\"accounts\""),
            "a principal holding both codes must see the accounts block: {page:.600}"
        );
        println!(
            "[fragment] both codes -> GET / answers 200; sidebar anchor:\n{}\ncard div:\n{}",
            around(&page, "data-nav=\"accounts\""),
            around(&page, "id=\"accounts\"")
        );
    }

    fn count_key(html: &str, needle: &str) -> usize {
        html.matches(needle).count()
    }

    /// The raw HTML around one marker, for the report a human reads.
    fn around(html: &str, needle: &str) -> String {
        let at = html
            .find(needle)
            .unwrap_or_else(|| panic!("marker {needle:?} absent"));
        let start = at.saturating_sub(120);
        let end = (at + 420).min(html.len());
        let window = &html[start..end];
        let first = window.find('<').unwrap_or(0);
        window[first..].to_string()
    }

    async fn send(
        app: Router,
        method: &str,
        uri: &str,
        content_type: Option<&str>,
        body: String,
    ) -> (StatusCode, String) {
        let mut builder = test_support::with_cookie(Request::builder().method(method).uri(uri));
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
            "INSERT INTO customers (name, created_by) VALUES (?, ?) RETURNING id",
        )
        .bind("Regression Buyer")
        .bind(test_support::audit_actor_id(pool).await.unwrap())
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
        method: i64,
        amount: &str,
    ) -> (StatusCode, String) {
        post_json(
            app,
            &format!("/api/sales/{sale_id}/payments"),
            serde_json::json!({
                "method_id": method,
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
        // The web form attaches the ticked methods to the new account, so a
        // payment with one of them is accepted.
        let (status, body) = web_create_account(&app, "Wallet", &[cash]).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "web create: {status} {body}");

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

        let (status, body) = pay(&app, sale, cash, "10").await;
        assert_eq!(
            status,
            StatusCode::CREATED,
            "payment must be accepted after web creation with the method: {body}"
        );
        let payments: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sale_payments WHERE sale_id = ?")
            .bind(sale)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(payments.0, 1, "payment row persisted");
    }

    #[tokio::test]
    async fn payment_rejected_with_actionable_message_when_method_unassigned() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);

        // REST-created accounts own nothing; Cash is unassigned too.
        let (status, body) = post_json(
            &app,
            "/api/accounts",
            serde_json::json!({ "name": "Unconfigured" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "create account: {body}");
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

        let (status, body) = pay(&app, sale, cash, "10").await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "must stay a 400 Validation: {body}"
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let msg = v["error"].as_str().unwrap_or_default();
        assert!(
            msg.contains("not assigned to any account"),
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
    async fn web_create_account_without_ticked_methods_creates_flagged_account() {
        let state = test_state().await;
        let pool = state.pool.clone();
        let app = crate::routes::router(state);

        // No ticks is allowed: the account is created method-less and the list
        // flags it with the actionable warning.
        let (status, body) = web_create_account(&app, "NoMethods", &[]).await;
        assert_eq!(status, StatusCode::SEE_OTHER, "{body}");
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM accounts WHERE name = 'NoMethods'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count.0, 1, "the account is created without methods");
        let (status, list) = send(app.clone(), "GET", "/web/accounts", None, String::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            list.contains("No payment methods configured"),
            "the method-less account must be flagged: {list}"
        );
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
        let owned: Vec<&str> = v["methods"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert_eq!(owned, vec!["Transfer"], "Cash must be unassigned: {body}");

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
        // Cash is now unassigned, so it renders in the unassigned section,
        // unchecked.
        assert!(
            page.contains(&format!("value=\"{cash}\"")),
            "Cash must still be offered as unassigned"
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
