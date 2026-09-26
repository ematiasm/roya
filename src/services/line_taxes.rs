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
//!
//! [`calculate_line_taxes`] is TOTAL: it returns a typed
//! [`PriceRefusal`] instead of panicking, and every arithmetic step in it is
//! checked, the final add included. That totality is what makes the single
//! route above safe to have — an unbounded `qty * price` and a stored rate an
//! admin chose both arrive here, and there is no `catch_unwind` anywhere in
//! this crate to stand between either of them and a dropped connection.
use rust_decimal::{Decimal, RoundingStrategy};

use crate::models::{NewLineTax, PriceRefusal, Tax};

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
///
/// `PartialEq` is derived for the consumers that compare two CALCULATIONS rather
/// than two rows: the final-price solve publishes this breakdown, and proving
/// the conversion is idempotent means proving the whole answer is unchanged,
/// breakdown included.
#[derive(Debug, Clone, PartialEq)]
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

/// The NET amount of one document line: `qty * price`, checked.
///
/// This lives beside [`calculate_line_taxes`] and not inside it because the
/// refusal it raises is a DIFFERENT rule with a different remedy. The contract
/// cannot see this product at all — its `net_subtotal` argument is already a
/// `Decimal`, so an amount that never fit is refused by whoever COMBINED the
/// operands, which is the document line's writer. An operator fixes this one by
/// lowering the quantity or the unit price; the tax arithmetic's own refusal
/// can also be fixed by the rate.
///
/// It is the ONE definition of that product, so the sale and the purchase writer
/// cannot drift on where the line's net amount comes from. The read-side twin
/// is `SaleLine::subtotal` / `PurchaseLine::subtotal`, which read the STORED
/// `qty` and price back; a write that refuses to store an unrepresentable
/// product is what keeps that multiply inside the range.
pub fn line_net_amount(qty: Decimal, price: Decimal) -> Result<Decimal, PriceRefusal> {
    qty.checked_mul(price)
        .ok_or(PriceRefusal::LineAmountTooLarge)
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
/// # Total by construction
///
/// This function REFUSES rather than panics. rust_decimal's raw `*` and `+`
/// operators panic on overflow, and every operand here arrives from a caller
/// that an operator or an admin controls: a document line's net is
/// `qty * price` with no ceiling on either field, and a rate is a stored column
/// an admin typed. A panic inside a handler escapes the task, the connection
/// is dropped, and the operator gets no response at all.
///
/// The refusal is [`PriceRefusal::TaxArithmeticTooLarge`], and it is returned
/// by a `Result` rather than offered as a checked helper: a helper would leave
/// every raw operator at every call site exactly as it was, while a `Result`
/// makes the guard structural — a future caller cannot forget it, because the
/// signature will not compile until they handle it.
///
/// # Every step is checked, INCLUDING the final add
///
/// The last step is the one that a multiply-only guard misses. There are
/// amounts for which every individual `net * rate` fits, the running tax total
/// fits, and only `net + tax_total` leaves the range: the top of the `Decimal`
/// range with a single 1% rate multiplies to the top of the range again, the
/// contribution is a hundredth of it, and adding a hundredth of the maximum to
/// the maximum overflows. The governing bound is therefore the PAIR
/// `net * (1 + SUM rate_i/100) <= MAX`, never the per-multiply
/// `net * rate <= MAX`, and the checked add is what enforces it. For a
/// non-negative rate set the pair is the tighter of the two.
///
/// No ceiling is hand-rolled to pre-empt any of this: a separate bound would be
/// a second place where the arithmetic's limit is stated, and it would have to
/// be re-derived every time the formula moved. The checked operators ARE the
/// bound.
pub fn calculate_line_taxes(
    net_subtotal: Decimal,
    taxes: &[Tax],
) -> Result<LineTaxCalculation, PriceRefusal> {
    let mut contributions = Vec::with_capacity(taxes.len());
    let mut tax_total = Decimal::ZERO;

    for tax in taxes {
        // `amount = net * rate / 100`, then pinned to cents. `to_f64` is not
        // involved: the division is exact decimal arithmetic.
        let raw = net_subtotal
            .checked_mul(tax.rate)
            .and_then(|product| product.checked_div(percent()))
            .ok_or(PriceRefusal::TaxArithmeticTooLarge)?;
        let amount = round_to_cents(raw);
        tax_total = tax_total
            .checked_add(amount)
            .ok_or(PriceRefusal::TaxArithmeticTooLarge)?;
        contributions.push(LineTaxSnapshot {
            tax_id: tax.id,
            code: tax.code.clone(),
            name: tax.name.clone(),
            rate: tax.rate,
            amount,
        });
    }

    // The add the probe isolated. Checked for the reason the doc above gives,
    // and it is the step that actually fails on the inputs a multiply-only guard
    // waves through.
    let total = net_subtotal
        .checked_add(tax_total)
        .ok_or(PriceRefusal::TaxArithmeticTooLarge)?;

    Ok(LineTaxCalculation {
        net_subtotal,
        taxes: contributions,
        // The sum of values that are already at `MONEY_SCALE` is exact, so it
        // needs no second rounding.
        tax_total,
        total: round_to_cents(total),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    /// The contract reads no clock, so a test `Tax` needs a timestamp only
    /// because the struct has the field. One constant for all of them.
    fn timestamp() -> chrono::NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
    }

    /// A resolved tax definition, at a rate the caller chose. The contract reads
    /// nothing else off a `Tax`: id, code, name and rate are the whole input,
    /// and a rate is the operand that decides whether the arithmetic is
    /// carryable.
    fn tax(rate: &str) -> Tax {
        Tax {
            id: 1,
            code: "IVA".into(),
            name: "IVA".into(),
            rate: Decimal::from_str(rate).unwrap(),
            is_active: true,
            created_by: 1,
            updated_by: None,
            created_at: timestamp(),
            updated_at: timestamp(),
        }
    }

    fn dec(raw: &str) -> Decimal {
        Decimal::from_str(raw).unwrap()
    }

    /// THE PROBE. Every individual multiply fits, the running tax total fits,
    /// and only the FINAL ADD leaves the range — so a guard that checked the
    /// multiplies and forgot the add would still crash on this input.
    ///
    /// The proof that this is the case is IN the test rather than in its name:
    /// the three intermediate steps are evaluated in their checked form first and
    /// asserted to be `Some`, and only the add is asserted to be `None`. If
    /// rust_decimal's limits ever move, this test stops claiming the case exists
    /// instead of quietly testing a different one.
    #[test]
    fn the_contract_refuses_when_only_the_final_add_leaves_the_range() {
        // The top of the `Decimal` range, with a single 1% rate.
        let net = Decimal::MAX;

        // Step 1 — `net * rate` FITS. This is the whole point: `MAX * 1` is
        // `MAX`.
        let product = net.checked_mul(dec("1"));
        assert!(
            product.is_some(),
            "the multiply must fit for this to be probe C"
        );
        // Step 2 — `/ 100` FITS.
        let scaled = product.unwrap().checked_div(percent());
        assert!(
            scaled.is_some(),
            "the division must fit for this to be probe C"
        );
        // Step 3 — the running tax total FITS.
        let running = Decimal::ZERO.checked_add(round_to_cents(scaled.unwrap()));
        assert!(
            running.is_some(),
            "the tax total must fit for this to be probe C"
        );
        // Step 4 — and only now, the add the multiply-only fix skips.
        assert!(
            net.checked_add(running.unwrap()).is_none(),
            "the final add must be the step that fails, or this is not probe C"
        );

        assert_eq!(
            calculate_line_taxes(net, &[tax("1")]).unwrap_err(),
            PriceRefusal::TaxArithmeticTooLarge
        );
    }

    /// The other way the same rule fires: the multiply itself. Kept as its own
    /// test because the two sites are independent — checking one and not the
    /// other is exactly the defect this work unit exists to close.
    #[test]
    fn the_contract_refuses_when_the_contribution_multiply_leaves_the_range() {
        // `MAX * 21` is a hundred and one times too large, and rust_decimal
        // cannot rescale a scale-0 product, so the multiply itself is the step
        // that fails. The refusal is the same rule: the amount is
        // representable, the PAIR is not.
        assert!(Decimal::MAX.checked_mul(dec("21")).is_none());
        assert_eq!(
            calculate_line_taxes(Decimal::MAX, &[tax("21")]).unwrap_err(),
            PriceRefusal::TaxArithmeticTooLarge
        );
    }

    /// THE BOUNDARY, pinned exactly, because the bound is the pair
    /// `net * (1 + rate/100) <= MAX` and a bound nobody can point at is a
    /// blanket rejection wearing a bound's clothes.
    ///
    /// At a 1% rate the largest net whose tax total can still be added to it is
    /// `78443725261647859003508861718`, whose total is EXACTLY `Decimal::MAX`:
    /// the largest money this contract can represent at all. One cent more and
    /// the add has nowhere to land, so it is refused — and the refusal is the
    /// same rule the pair states, not a separate ceiling.
    #[test]
    fn the_largest_carriable_net_computes_and_one_cent_more_is_refused() {
        let largest = dec("78443725261647859003508861718");
        let one_cent_over = dec("78443725261647859003508861719");

        // Stated as arithmetic, not as a magic number: the tax total is
        // `net / 100` exactly at 1%, and the sum is the ceiling.
        let tax_total = round_to_cents(
            largest
                .checked_mul(dec("1"))
                .unwrap()
                .checked_div(percent())
                .unwrap(),
        );
        assert_eq!(largest.checked_add(tax_total), Some(Decimal::MAX));

        let calc =
            calculate_line_taxes(largest, &[tax("1")]).expect("the largest value is carried");
        assert_eq!(calc.net_subtotal, largest);
        assert_eq!(calc.tax_total, tax_total);
        assert_eq!(calc.total, Decimal::MAX, "the largest money there is");

        assert_eq!(
            calculate_line_taxes(one_cent_over, &[tax("1")]).unwrap_err(),
            PriceRefusal::TaxArithmeticTooLarge
        );
    }

    /// THE CONTROL on the other side: an untaxed line at the very top of the
    /// range still computes. With no rate there is no pair to overflow, so the
    /// refusal must not fire — a contract that refused everything large would
    /// satisfy the boundary test above and be useless.
    #[test]
    fn an_untaxed_line_at_the_very_top_of_the_range_still_computes() {
        let calc = calculate_line_taxes(Decimal::MAX, &[]).expect("no rate, no pair, no refusal");
        assert_eq!(calc.net_subtotal, Decimal::MAX);
        assert_eq!(calc.tax_total, Decimal::ZERO);
        assert_eq!(calc.total, Decimal::MAX);
    }

    /// A zero-rate tax is a real linked tax: the arithmetic still runs, and the
    /// contract still says so. It must not be mistaken for an overflow.
    #[test]
    fn a_zero_rate_tax_at_the_top_of_the_range_still_computes() {
        let calc = calculate_line_taxes(Decimal::MAX, &[tax("0")]).expect("a 0% contribution is 0");
        assert_eq!(calc.tax_total, Decimal::ZERO);
        assert_eq!(calc.total, Decimal::MAX);
    }

    /// The line amount is `qty * price`, and the refusal for IT is a different
    /// rule with a different remedy — the operator lowers the quantity or the
    /// price, not a tax. Its own boundary, pinned the same way: `1 * MAX` is the
    /// largest line amount there is, and `2 * MAX` is one step past it.
    #[test]
    fn the_line_amount_itself_has_its_own_rule_and_its_own_boundary() {
        assert_eq!(line_net_amount(dec("1"), Decimal::MAX), Ok(Decimal::MAX));
        assert_eq!(
            line_net_amount(dec("2"), Decimal::MAX),
            Err(PriceRefusal::LineAmountTooLarge)
        );
        // The 1e20 * 1e9 an operator types into a quantity box with a price.
        assert_eq!(
            line_net_amount(dec("100000000000000000000"), dec("1000000000")),
            Err(PriceRefusal::LineAmountTooLarge)
        );
        // The two rules are distinct, so a caller can never confuse them.
        assert_ne!(
            PriceRefusal::LineAmountTooLarge,
            PriceRefusal::TaxArithmeticTooLarge
        );
    }

    /// A representable amount and a representable rate are still computed
    /// exactly as before, contribution by contribution, additively. The checked
    /// operators changed WHICH inputs are refused, never the money.
    #[test]
    fn an_ordinary_amount_and_rate_set_is_computed_unchanged() {
        let calc = calculate_line_taxes(dec("100"), &[tax("21"), tax("10")]).unwrap();
        assert_eq!(calc.taxes[0].amount, dec("21"));
        assert_eq!(calc.taxes[1].amount, dec("10"));
        assert_eq!(calc.tax_total, dec("31"));
        assert_eq!(calc.total, dec("131"));
    }
}
