pub mod api;
pub mod customers_api;
pub mod customers_web;
pub mod inventory_api;
pub mod inventory_web;
pub mod purchases_api;
pub mod purchases_web;
pub mod sales_api;
pub mod sales_web;
pub mod suppliers_web;
pub mod web;

use axum::{http::StatusCode, response::IntoResponse, Json, Router};
use sqlx::SqlitePool;
use tower_http::services::ServeDir;

use crate::repositories::{
    SqliteAccountRepository, SqliteBarcodeRepository, SqliteCategoryRepository,
    SqliteCustomerReceiptRepository, SqliteCustomerRepository, SqliteDocSequenceRepository,
    SqlitePaymentMethodRepository, SqliteProductRepository,
    SqliteProductSupplierCostRepository, SqlitePurchaseRepository, SqliteSaleRepository,
    SqliteStockMovementRepository, SqliteSupplierRepository, SqliteTransactionRepository,
};
use crate::services::{
    AccountService, CustomerReceiptService, CustomerService, InventoryService,
    PaymentMethodService, PurchasesService, SalesService, SupplierService, TransactionService,
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
    SqliteCustomerRepository,
>;

pub type CustomerSvc = CustomerService<SqliteCustomerRepository>;

/// Receipts: the grouped payments of one handover of money. It wraps the same
/// sales service the routes use, so every grouped payment reaches sales and
/// finance exactly like any other payment.
pub type ReceiptSvc = CustomerReceiptService<
    SqliteCustomerReceiptRepository,
    SqliteSaleRepository,
    SqliteDocSequenceRepository,
    SqliteCategoryRepository,
    SqliteProductRepository,
    SqliteBarcodeRepository,
    SqliteStockMovementRepository,
    SqliteAccountRepository,
    SqliteTransactionRepository,
    SqlitePaymentMethodRepository,
    SqliteCustomerRepository,
>;

pub type MethodSvc = PaymentMethodService<SqlitePaymentMethodRepository>;

pub type SupplierSvc =
    SupplierService<SqliteSupplierRepository, SqliteProductSupplierCostRepository>;

pub type PurchasesSvc = PurchasesService<
    SqlitePurchaseRepository,
    SqliteDocSequenceRepository,
    SqliteSupplierRepository,
    SqliteProductSupplierCostRepository,
    SqliteCategoryRepository,
    SqliteProductRepository,
    SqliteBarcodeRepository,
    SqliteStockMovementRepository,
    SqliteAccountRepository,
    SqliteTransactionRepository,
    SqlitePaymentMethodRepository,
>;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub account_service: AccountService<SqliteAccountRepository, SqliteTransactionRepository>,
    pub transaction_service:
        TransactionService<SqliteAccountRepository, SqliteTransactionRepository>,
    pub inventory_service: InventorySvc,
    pub sales_service: SalesSvc,
    pub customer_service: CustomerSvc,
    pub customer_receipt_service: ReceiptSvc,
    pub payment_method_service: MethodSvc,
    pub supplier_service: SupplierSvc,
    pub purchases_service: PurchasesSvc,
    pub allow_negative: bool,
    pub allow_negative_stock: bool,
    /// `ENFORCE_CREDIT_LIMIT` (default true): the sales service rejects a credit
    /// confirm whose projected debt exceeds the customer's limit.
    pub enforce_credit_limit: bool,
}

impl AppState {
    /// Compatibility constructor: credit-limit enforcement defaults to true,
    /// exactly like `main` when the env var is absent.
    pub fn new(pool: SqlitePool, allow_negative: bool, allow_negative_stock: bool) -> Self {
        Self::new_with_credit_limit(pool, allow_negative, allow_negative_stock, true)
    }

    pub fn new_with_credit_limit(
        pool: SqlitePool,
        allow_negative: bool,
        allow_negative_stock: bool,
        enforce_credit_limit: bool,
    ) -> Self {
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
        let customer_service =
            CustomerService::new(SqliteCustomerRepository::new(pool.clone()));
        let sales_service = SalesService::new(
            SqliteSaleRepository::new(pool.clone()),
            SqliteDocSequenceRepository::new(pool.clone()),
            inventory_service.clone(),
            transaction_service.clone(),
            method_repo.clone(),
            customer_service.clone(),
            enforce_credit_limit,
        );
        // M4: collections group the payments one handover of money produced; the
        // receipt service composes the same sales service and the finance-owned
        // (account, method) allowlist the rest of the app uses.
        let customer_receipt_service = CustomerReceiptService::new(
            SqliteCustomerReceiptRepository::new(pool.clone()),
            sales_service.clone(),
            payment_method_service.clone(),
        );
        // M3: suppliers + product/supplier cost satellite are consumed by the
        // purchases orchestrator; both share the same SQLite repos as the rest
        // of the app.
        let supplier_service = SupplierService::new(
            SqliteSupplierRepository::new(pool.clone()),
            SqliteProductSupplierCostRepository::new(pool.clone()),
        );
        let purchases_service = PurchasesService::new(
            SqlitePurchaseRepository::new(pool.clone()),
            SqliteDocSequenceRepository::new(pool.clone()),
            supplier_service.clone(),
            inventory_service.clone(),
            transaction_service.clone(),
            PaymentMethodService::new(method_repo),
        );
        Self {
            pool,
            account_service,
            transaction_service,
            inventory_service,
            sales_service,
            customer_service,
            customer_receipt_service,
            payment_method_service,
            supplier_service,
            purchases_service,
            allow_negative,
            allow_negative_stock,
            enforce_credit_limit,
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(api::router())
        .merge(web::router())
        .merge(customers_api::router())
        .merge(customers_web::router())
        .merge(inventory_api::router())
        .merge(inventory_web::router())
        .merge(sales_api::router())
        .merge(sales_web::router())
        .merge(purchases_api::router())
        .merge(purchases_web::router())
        .merge(suppliers_web::router())
        .nest_service("/static", ServeDir::new("static"))
        .fallback(route_not_found)
        .with_state(state)
}

/// Distinctive body for the router-level 404 fallback. A routing miss must be
/// distinguishable from a handler-level 404 (which returns its own message),
/// so the smoke suite can use this marker as an oracle for the routing table.
async fn route_not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "route not found" })),
    )
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    use tower::ServiceExt;

    use super::{router, AppState};

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
        AppState::new(pool, false, true)
    }

    #[tokio::test]
    async fn static_assets_are_served_from_disk() {
        let app = router(test_state().await);
        for uri in ["/static/htmx.min.js", "/static/tailwind.css"] {
            let req = Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri} should be served");
        }
    }
}
