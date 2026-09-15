pub mod api;
pub mod inventory_api;
pub mod inventory_web;
pub mod web;

use axum::Router;
use sqlx::SqlitePool;

use crate::repositories::{
    SqliteAccountRepository, SqliteBarcodeRepository, SqliteCategoryRepository,
    SqliteProductRepository, SqliteStockMovementRepository, SqliteTransactionRepository,
};
use crate::services::{AccountService, InventoryService, TransactionService};

pub type InventorySvc = InventoryService<
    SqliteCategoryRepository,
    SqliteProductRepository,
    SqliteBarcodeRepository,
    SqliteStockMovementRepository,
>;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub account_service: AccountService<SqliteAccountRepository, SqliteTransactionRepository>,
    pub transaction_service:
        TransactionService<SqliteAccountRepository, SqliteTransactionRepository>,
    pub inventory_service: InventorySvc,
    pub allow_negative: bool,
    pub allow_negative_stock: bool,
}

impl AppState {
    pub fn new(pool: SqlitePool, allow_negative: bool, allow_negative_stock: bool) -> Self {
        let acc_repo = SqliteAccountRepository::new(pool.clone());
        let tx_repo = SqliteTransactionRepository::new(pool.clone());
        let account_service = AccountService::new(acc_repo.clone(), tx_repo.clone());
        let transaction_service =
            TransactionService::new(acc_repo.clone(), tx_repo.clone(), allow_negative);
        let inventory_service = InventoryService::new(
            SqliteCategoryRepository::new(pool.clone()),
            SqliteProductRepository::new(pool.clone()),
            SqliteBarcodeRepository::new(pool.clone()),
            SqliteStockMovementRepository::new(pool.clone()),
            allow_negative_stock,
        );
        Self {
            pool,
            account_service,
            transaction_service,
            inventory_service,
            allow_negative,
            allow_negative_stock,
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(api::router())
        .merge(web::router())
        .merge(inventory_api::router())
        .merge(inventory_web::router())
        .with_state(state)
}
