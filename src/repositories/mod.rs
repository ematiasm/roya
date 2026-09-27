pub mod account_repo;
pub mod barcode_repo;
pub mod business_config_repo;
pub mod category_repo;
pub mod customer_receipt_repo;
pub mod customer_repo;
pub mod doc_sequence_repo;
pub mod payment_method_repo;
pub mod permission_repo;
pub mod product_repo;
pub mod product_supplier_cost_repo;
pub mod purchase_repo;
// role_repo's first production consumer is the S2 bootstrap: the identity
// service holds the repository and grants the protected role (services/identity.rs),
// so the re-exports are wired now; the S3 user admin and the S4 roles admin
// consume the remaining methods.
pub mod role_repo;
pub mod sale_repo;
pub mod session_repo;
/// Add a SET of stored amounts, refusing when the running sum leaves the range.
///
/// This is the repository layer's copy of the argument the service layer keeps in
/// `checked_money_sum`, and it exists because the rows these folds read are
/// written one at a time: a bounded write says nothing about the sum of a set of
/// them, which is the same reason a document total is checked separately from a
/// line. `PriceRefusal::AggregateTooLarge` is the rule, and it is not
/// `DocumentTotalTooLarge` because these sets are an account's transactions and a
/// product's movements, not one document's lines.
pub fn checked_aggregate_sum<'a>(
    amounts: impl IntoIterator<Item = &'a Decimal>,
) -> Result<Decimal, crate::models::PriceRefusal> {
    let mut sum = Decimal::ZERO;
    for amount in amounts {
        sum = sum
            .checked_add(*amount)
            .ok_or(crate::models::PriceRefusal::AggregateTooLarge)?;
    }
    Ok(sum)
}

pub mod setup_repo;
pub mod stock_repo;
pub mod supplier_repo;
pub mod tax_repo;
pub mod tax_snapshot_repo;
pub mod transaction_repo;
pub mod user_repo;

pub use account_repo::{AccountRepository, SqliteAccountRepository};
pub use barcode_repo::{BarcodeRepository, SqliteBarcodeRepository};
pub use business_config_repo::{
    BusinessConfigurationRepository, BusinessLocaleRepository, BusinessSettingsRepository,
    SqliteBusinessConfigurationRepository, SqliteBusinessLocaleRepository,
    SqliteBusinessSettingsRepository,
};
pub use category_repo::{CategoryRepository, SqliteCategoryRepository};
pub use customer_receipt_repo::{CustomerReceiptRepository, SqliteCustomerReceiptRepository};
pub use customer_repo::{CustomerRepository, SqliteCustomerRepository};
pub use doc_sequence_repo::{DocSequenceRepository, SqliteDocSequenceRepository};
pub use payment_method_repo::{PaymentMethodRepository, SqlitePaymentMethodRepository};
pub use permission_repo::{PermissionRepository, SqlitePermissionRepository};
pub use product_repo::{ProductRepository, SqliteProductRepository};
pub use product_supplier_cost_repo::{
    ProductSupplierCostRepository, SqliteProductSupplierCostRepository,
};
pub use purchase_repo::{PurchaseRepository, SqlitePurchaseRepository};
pub use role_repo::{RoleRepository, SqliteRoleRepository};
pub use sale_repo::{SaleRepository, SqliteSaleRepository};
pub use session_repo::{SessionRepository, SqliteSessionRepository};
pub use setup_repo::{SetupRecord, SetupRepository, SqliteSetupRepository};
pub use stock_repo::{SqliteStockMovementRepository, StockMovementRepository};
pub use supplier_repo::{SqliteSupplierRepository, SupplierRepository};
pub use tax_repo::{
    ProductTaxRepository, SqliteProductTaxRepository, SqliteTaxRepository, TaxRepository,
};
pub use tax_snapshot_repo::{SqliteTaxSnapshotRepository, TaxSnapshotRepository};
pub use transaction_repo::{SqliteTransactionRepository, TransactionRepository};
pub use user_repo::{SqliteUserRepository, UserRepository};

use rust_decimal::Decimal;

/// Helper: compute signed amount for a transaction kind.
pub fn signed_amount(kind: crate::models::TransactionKind, amount: Decimal) -> Decimal {
    match kind {
        crate::models::TransactionKind::Income => amount,
        crate::models::TransactionKind::Expense => -amount,
    }
}
