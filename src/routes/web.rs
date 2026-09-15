use askama::Template;
use axum::{
    extract::{Form, Path, Query, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect},
    routing::{delete, get},
    Router,
};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::models::TransactionKind;
use crate::repositories::AccountRepository;
use crate::routes::AppState;

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
}

#[derive(Template)]
#[template(path = "account_detail.html")]
struct AccountDetailTemplate {
    account: crate::models::AccountWithBalance,
    transactions: Vec<crate::models::Transaction>,
    allow_negative: bool,
}

#[derive(Template)]
#[template(path = "partials/account_list.html")]
struct AccountListPartial {
    accounts: Vec<crate::models::AccountWithBalance>,
    total_balance: Decimal,
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .map(|v| v == "true")
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn dashboard(
    State(state): State<AppState>,
) -> Result<Html<String>, AppError> {
    let accounts = state.account_service.list_with_balances().await?;
    let total_balance = state.account_service.total_balance().await?;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let tmpl = DashboardTemplate {
        accounts,
        total_balance,
        allow_negative: state.allow_negative,
        today,
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
    let tmpl = AccountDetailTemplate {
        account: acc_with_balance,
        transactions: detail.transactions.clone(),
        allow_negative: state.allow_negative,
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
#[derive(Debug, Deserialize)]
pub struct CreateAccountForm {
    pub name: String,
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
    let _acc = state.account_service.create(&form.name).await?;
    // If HTMX, return updated fragments
    if is_htmx(&headers) {
        let accounts = state.account_service.list_with_balances().await?;
        let total = state.account_service.total_balance().await?;
        let list_html = AccountListPartial {
            accounts: accounts.clone(),
            total_balance: total,
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
        .route("/web/accounts", get(web_account_list).post(web_create_account))
        .route("/web/account-options", get(web_account_options))
        .route(
            "/web/transactions",
            get(web_transaction_list).post(web_create_transaction),
        )
        .route("/web/transactions/{id}", delete(web_delete_transaction))
}
