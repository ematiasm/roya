pub mod account_repo;
pub mod barcode_repo;
pub mod category_repo;
pub mod doc_sequence_repo;
pub mod product_repo;
pub mod sale_repo;
pub mod stock_repo;
pub mod transaction_repo;

pub use account_repo::{AccountRepository, SqliteAccountRepository};
pub use barcode_repo::{BarcodeRepository, SqliteBarcodeRepository};
pub use category_repo::{CategoryRepository, SqliteCategoryRepository};
pub use doc_sequence_repo::{DocSequenceRepository, SqliteDocSequenceRepository};
pub use product_repo::{ProductRepository, SqliteProductRepository};
pub use sale_repo::{SaleRepository, SqliteSaleRepository};
pub use stock_repo::{SqliteStockMovementRepository, StockMovementRepository};
pub use transaction_repo::{SqliteTransactionRepository, TransactionRepository};

use rust_decimal::Decimal;

/// Helper: compute signed amount for a transaction kind.
pub fn signed_amount(kind: crate::models::TransactionKind, amount: Decimal) -> Decimal {
    match kind {
        crate::models::TransactionKind::Income => amount,
        crate::models::TransactionKind::Expense => -amount,
    }
}
