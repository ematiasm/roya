use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json, Router,
    routing::{get, put},
};

use crate::error::AppResult;
use crate::models::{CreateAccountRequest, CreateTransactionRequest, TransactionFilter, UpdateTransactionRequest};
use crate::routes::AppState;

async fn list_accounts(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    let accounts = state.account_service.list_with_balances().await?;
    let total = state.account_service.total_balance().await?;
    Ok(Json(serde_json::json!({ "accounts": accounts, "total_balance": total })))
}

async fn create_account(
    State(state): State<AppState>,
    Json(payload): Json<CreateAccountRequest>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let acc = state.account_service.create(&payload.name).await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(acc))))
}

async fn get_account(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let detail = state.account_service.get_detail(id).await?;
    Ok(Json(serde_json::json!(detail)))
}

async fn list_transactions(
    State(state): State<AppState>,
    Query(filter): Query<TransactionFilter>,
) -> AppResult<Json<serde_json::Value>> {
    let txs = state.transaction_service.list(filter).await?;
    Ok(Json(serde_json::json!({ "transactions": txs })))
}

async fn create_transaction(
    State(state): State<AppState>,
    Json(payload): Json<CreateTransactionRequest>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let tx = state
        .transaction_service
        .create(
            payload.account_id,
            payload.kind,
            payload.amount,
            payload.description,
            payload.date,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(serde_json::json!(tx))))
}

async fn update_transaction(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateTransactionRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let tx = state
        .transaction_service
        .update(id, payload.kind, payload.amount, payload.description, payload.date)
        .await?;
    Ok(Json(serde_json::json!(tx)))
}

async fn delete_transaction(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    state.transaction_service.delete(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/accounts", get(list_accounts).post(create_account))
        .route("/api/accounts/{id}", get(get_account))
        .route("/api/transactions", get(list_transactions).post(create_transaction))
        .route(
            "/api/transactions/{id}",
            put(update_transaction).delete(delete_transaction),
        )
}
