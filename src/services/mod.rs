pub mod account;
pub mod customer_receipts;
pub mod customers;
pub mod documents;
pub mod final_price;
pub mod finance_methods;
pub mod gross_inverse;
pub mod identity;
pub mod inventory;
pub mod line_taxes;
pub mod purchases;
pub mod sales;
pub mod settings;
pub mod setup;
pub mod suppliers;
pub mod taxes;
pub mod transaction;

use rust_decimal::Decimal;

use crate::models::PriceRefusal;

/// One checked addition to a running money total, or the document-total refusal.
///
/// This exists because `Decimal`'s raw `+` panics, and every money figure in
/// this application is a sum of something an operator or an admin controls. It
/// is used for the sums built FROM documents rather than inside one — a
/// customer's outstanding balance, a debt banner's total, an ageing bucket —
/// which is the one level a per-line bound and a per-document fold cannot see:
/// every document's total is representable and the set of them is not. Two
/// documents of `4e28` are the smallest such case.
///
/// It returns a `Result` for the same reason the document fold does: a caller
/// cannot forget the guard, because the signature will not compile until they
/// handle it. The refusal is [`PriceRefusal::DocumentTotalTooLarge`] because no
/// single document and no single line is at fault — the operator's remedy is on
/// the documents, not on one number.
pub fn checked_money_add(total: Decimal, figure: Decimal) -> Result<Decimal, PriceRefusal> {
    total
        .checked_add(figure)
        .ok_or(PriceRefusal::DocumentTotalTooLarge)
}

/// [`checked_money_add`] over a set of figures, for the places that would
/// otherwise reach for `Iterator::sum` — which is the same raw fold with the
/// same panic, spelled shorter.
pub fn checked_money_sum<'a>(
    figures: impl IntoIterator<Item = &'a Decimal>,
) -> Result<Decimal, PriceRefusal> {
    let mut sum = Decimal::ZERO;
    for figure in figures {
        sum = checked_money_add(sum, *figure)?;
    }
    Ok(sum)
}

pub use account::AccountService;
pub use customer_receipts::CustomerReceiptService;
pub use customers::CustomerService;
pub use documents::DocumentService;
pub use finance_methods::PaymentMethodService;
pub use identity::IdentityService;
pub use inventory::InventoryService;
pub use purchases::PurchasesService;
pub use sales::SalesService;
pub use settings::SettingsService;
pub use setup::SetupService;
pub use suppliers::SupplierService;
pub use taxes::TaxService;
pub use transaction::TransactionService;
