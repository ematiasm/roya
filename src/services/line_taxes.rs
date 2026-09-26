//! The shared, PURE line-tax calculation contract (tax calculation and
//! settings, T1).
//!
//! One definition of "how much tax does this document line owe", consumed
//! identically by the sales flow, the purchases flow and the product
//! tax-inclusive price preview. The module opens no connection and reads no
//! clock: it takes a net `Decimal` and the tax definitions already resolved
//! for the product, and returns the immutable facts to snapshot plus the
//! rounded money. Everything a document persists about taxes comes from here,
//! so two flows can never disagree by a cent.
//!
//! The rules, in one place:
//!
//! * A tax `rate` is a PERCENTAGE of the net subtotal (`rate = 21` means 21%).
//! * Taxes are ADDITIVE, never compounding: every tax is applied to the same
//!   net subtotal, so 21% + 10% on 100 is 31, not 132.10.
//! * Each contribution is pinned to two decimals with HALF-UP rounding
//!   (midpoint away from zero), the same `RoundingStrategy` the product
//!   markup pricing slice established for derived money. The tax total is the
//!   sum of those pinned contributions, so a stored breakdown always
//!   reconciles with the stored tax total, and the tax-inclusive total is the
//!   net plus that sum, rounded half-up as well.
//!
//! Inactive taxes are NOT filtered here: exclusion is a RESOLUTION concern and
//! belongs to the repository read that answers "which taxes apply to this
//! product" (`ProductTaxRepository::list_active_for_product`). This function
//! calculates exactly the definitions it is given, which is what makes it
//! deterministic and testable in isolation.
//!
//! The document repositories' line writes call this on a real request path, so
//! there is NO way in this application to compute a document line's taxes that
//! does not come through here. The read side has the same single route:
//! [`tax_inclusive_total`] is the one place a stored line's net amount and its
//! stored tax amount are added together, so no total, payment ceiling or debt
//! balance derives its own arithmetic.
use rust_decimal::{Decimal, RoundingStrategy};

use crate::models::{NewLineTax, Tax};

/// The currency's decimal places. Every money this feature derives is pinned
/// to this scale before it is stored or shown.
pub const MONEY_SCALE: u32 = 2;

/// One hundred, the divisor that turns a percentage rate into a factor.
fn percent() -> Decimal {
    Decimal::from(100)
}

/// Half-up money rounding: ties go AWAY from zero, so an exact midpoint
/// (`1.005`) becomes `1.01` and never `1.00` the way banker's rounding would.
/// Negative amounts round symmetrically (`-1.005` becomes `-1.01`), which keeps
/// a refund or reversal line consistent with its positive counterpart.
///
/// This is the single half-up money rule of the tax feature: contributions,
/// tax totals and tax-inclusive totals all route through it.
pub fn round_to_cents(amount: Decimal) -> Decimal {
    amount.round_dp_with_strategy(MONEY_SCALE, RoundingStrategy::MidpointAwayFromZero)
}

/// A line's tax calculation: the immutable per-tax facts to snapshot, plus the
/// money they add up to.
#[derive(Debug, Clone)]
pub struct LineTaxCalculation {
    /// The net amount the taxes were applied to, exactly as received.
    ///
    /// Read by the document totals and the product tax-inclusive preview.
    pub net_subtotal: Decimal,
    /// One entry per resolved tax, in the order it was resolved. Each entry is
    /// a snapshot fact: it never re-reads the tax.
    pub taxes: Vec<LineTaxSnapshot>,
    /// The sum of the contributions, already at `MONEY_SCALE`.
    pub tax_total: Decimal,
    /// The tax-inclusive line total at `MONEY_SCALE`, the single rounding rule
    /// applied once. Read by the same consumers as `net_subtotal`.
    pub total: Decimal,
}

/// The tax-inclusive money of ONE stored document line: the net subtotal plus the
/// tax total that line already froze, pinned to cents.
///
/// This is the read-side twin of [`calculate_line_taxes`]. A line's net
/// subtotal is `qty * price` and can carry more than two decimals (a fractional
/// quantity), so the sum is rounded here, exactly as the write path rounds it.
/// Every derived document figure routes through this one function, so a
/// document total, a payment ceiling and a debt balance can never disagree by a
/// cent about the same line.
///
/// Rounding belongs to the LINE, not to the document: a document total is the
/// sum of its lines' pinned totals, which is what the operator sees on the
/// record page and can therefore audit.
pub fn tax_inclusive_total(net_subtotal: Decimal, tax_total: Decimal) -> Decimal {
    round_to_cents(net_subtotal + tax_total)
}

/// One tax's contribution to a line, together with the values that produced
/// it. The write shape of this is [`NewLineTax`], which is what gets persisted
/// on the document line.
#[derive(Debug, Clone)]
pub struct LineTaxSnapshot {
    pub tax_id: i64,
    pub code: String,
    pub name: String,
    /// The rate used, as a percentage.
    pub rate: Decimal,
    /// `round_to_cents(net_subtotal * rate / 100)`.
    pub amount: Decimal,
}

impl From<&LineTaxSnapshot> for NewLineTax {
    /// The snapshot a line persists: the same facts, shaped for insertion into
    /// either family's snapshot table.
    fn from(snapshot: &LineTaxSnapshot) -> Self {
        NewLineTax {
            tax_id: snapshot.tax_id,
            code: snapshot.code.clone(),
            name: snapshot.name.clone(),
            rate: snapshot.rate,
            amount: snapshot.amount,
        }
    }
}

/// Calculate one document line's taxes.
///
/// `net_subtotal` is the NET amount (product sale price or purchase unit cost
/// times quantity — the user decision is that both are net). `taxes` are the
/// tax definitions already resolved for the product; every one of them is
/// applied to the same net subtotal, additively.
///
/// Returns the snapshot facts plus the rounded tax total and the rounded
/// tax-inclusive total. With no taxes the tax total is exactly zero and the
/// total is the net amount rounded to cents.
///
/// The arithmetic is plain `Decimal` math, exactly like `SaleLine::subtotal`:
/// `Decimal`'s own 28-digit range is the only guard against an absurd
/// net/rate pair, and a rate that large cannot reach a document line — the tax
/// service rejects a negative rate and an operator cannot type 28 digits into a
/// percentage field.
pub fn calculate_line_taxes(net_subtotal: Decimal, taxes: &[Tax]) -> LineTaxCalculation {
    let mut contributions = Vec::with_capacity(taxes.len());
    let mut tax_total = Decimal::ZERO;

    for tax in taxes {
        // `amount = net * rate / 100`, then pinned to cents. `to_f64` is not
        // involved: the division is exact decimal arithmetic.
        let raw = net_subtotal * tax.rate / percent();
        let amount = round_to_cents(raw);
        tax_total += amount;
        contributions.push(LineTaxSnapshot {
            tax_id: tax.id,
            code: tax.code.clone(),
            name: tax.name.clone(),
            rate: tax.rate,
            amount,
        });
    }

    LineTaxCalculation {
        net_subtotal,
        taxes: contributions,
        // The sum of values that are already at `MONEY_SCALE` is exact, so it
        // needs no second rounding.
        tax_total,
        total: round_to_cents(net_subtotal + tax_total),
    }
}
