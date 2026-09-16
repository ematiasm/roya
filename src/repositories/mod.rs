pub mod account_repo;
pub mod barcode_repo;
pub mod category_repo;
pub mod doc_sequence_repo;
pub mod payment_method_repo;
pub mod product_repo;
pub mod product_supplier_cost_repo;
pub mod purchase_repo;
pub mod sale_repo;
pub mod stock_repo;
pub mod supplier_repo;
pub mod transaction_repo;

pub use account_repo::{AccountRepository, SqliteAccountRepository};
pub use barcode_repo::{BarcodeRepository, SqliteBarcodeRepository};
pub use category_repo::{CategoryRepository, SqliteCategoryRepository};
pub use doc_sequence_repo::{DocSequenceRepository, SqliteDocSequenceRepository};
pub use payment_method_repo::{PaymentMethodRepository, SqlitePaymentMethodRepository};
pub use product_repo::{ProductRepository, SqliteProductRepository};
pub use product_supplier_cost_repo::{
    ProductSupplierCostRepository, SqliteProductSupplierCostRepository,
};
pub use purchase_repo::{PurchaseRepository, SqlitePurchaseRepository};
pub use sale_repo::{SaleRepository, SqliteSaleRepository};
pub use stock_repo::{SqliteStockMovementRepository, StockMovementRepository};
pub use supplier_repo::{SqliteSupplierRepository, SupplierRepository};
pub use transaction_repo::{SqliteTransactionRepository, TransactionRepository};

use rust_decimal::Decimal;

/// Helper: compute signed amount for a transaction kind.
pub fn signed_amount(kind: crate::models::TransactionKind, amount: Decimal) -> Decimal {
    match kind {
        crate::models::TransactionKind::Income => amount,
        crate::models::TransactionKind::Expense => -amount,
    }
}
