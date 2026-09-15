pub mod api;
pub mod inventory_api;
pub mod inventory_web;
pub mod sales_api;
pub mod sales_web;
pub mod web;

use axum::Router;
use sqlx::SqlitePool;

use crate::repositories::{
    SqliteAccountRepository, SqliteBarcodeRepository, SqliteCategoryRepository,
    SqliteDocSequenceRepository, SqlitePaymentMethodRepository, SqliteProductRepository,
    SqliteSaleRepository, SqliteStockMovementRepository, SqliteTransactionRepository,
};
use crate::services::{
    AccountService, InventoryService, PaymentMethodService, SalesService, TransactionService,
};

pub type InventorySvc = InventoryService<
    SqliteCategoryRepository,
    SqliteProductRepository,
    SqliteBarcodeRepository,
    SqliteStockMovementRepository,
>;

pub type SalesSvc = SalesService<
    SqliteSaleRepository,
    SqliteDocSequenceRepository,
    SqliteCategoryRepository,
    SqliteProductRepository,
    SqliteBarcodeRepository,
    SqliteStockMovementRepository,
    SqliteAccountRepository,
    SqliteTransactionRepository,
    SqlitePaymentMethodRepository,
>;

pub type MethodSvc = PaymentMethodService<SqlitePaymentMethodRepository>;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub account_service: AccountService<SqliteAccountRepository, SqliteTransactionRepository>,
    pub transaction_service:
        TransactionService<SqliteAccountRepository, SqliteTransactionRepository>,
    pub inventory_service: InventorySvc,
    pub sales_service: SalesSvc,
    pub payment_method_service: MethodSvc,
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
        let method_repo = SqlitePaymentMethodRepository::new(pool.clone());
        let payment_method_service = PaymentMethodService::new(method_repo.clone());
        let sales_service = SalesService::new(
            SqliteSaleRepository::new(pool.clone()),
            SqliteDocSequenceRepository::new(pool.clone()),
            inventory_service.clone(),
            transaction_service.clone(),
            method_repo,
        );
        Self {
            pool,
            account_service,
            transaction_service,
            inventory_service,
            sales_service,
            payment_method_service,
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
        .merge(sales_api::router())
        .merge(sales_web::router())
        .with_state(state)
}
