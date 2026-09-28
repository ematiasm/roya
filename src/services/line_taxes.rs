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

/// The money scale AS A VALUE: one hundredth of a unit, the smallest step the
/// rounding rule above is allowed to move.
///
/// It exists because [`MONEY_SCALE`] is a SCALE — a count of decimal places, a
/// rule to hand to a rounding call — while a caller that has to BUILD money at
/// that scale needs the amount itself: stepping a candidate by a hundredth, or
/// naming the smallest difference two prices are allowed to have. Those are not
/// rounding operations, so [`round_to_cents`] cannot answer them, and every
/// caller writing `Decimal::new(1, MONEY_SCALE)` for itself would be
/// re-deriving the step size of the currency in a second place.
///
/// It lives here, beside the constant it is built from, because this module
/// owns the money rules: [`MONEY_SCALE`] says how many places money has and
/// this says what one of those places is worth. Everything that moves money in
/// single-cent steps — this contract, and the gross-to-net search in
/// [`gross_inverse`](crate::services::gross_inverse) — takes this value rather
/// than reconstructing it, so the step size of the currency has exactly one
/// definition in the crate.
pub fn cent() -> Decimal {
    Decimal::new(1, MONEY_SCALE)
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
/// the maximum overflows. So the final add is checked, and the multiply is
/// checked, and the governing bound is whichever of the two is TIGHTER — never
/// one of them alone:
///
/// * the PAIR, `net * (1 + SUM rate_i/100) <= MAX`, which the checked running
///   add and the checked final add enforce, and
/// * the PER-MULTIPLY, `net * max(rate_i) <= MAX`, which the checked multiply
///   enforces.
///
/// Which of the two binds is not a matter of taste, and the naive reading of
/// "the pair is always tighter" is wrong. The multiply happens BEFORE the
/// division by 100 (see the loop below), so for a single rate `R` the two
/// bounds are `net <= MAX/(1 + R/100)` and `net <= MAX/R`, and the per-multiply
/// is the tighter one as soon as `R > 1/(1 - 1/100) ≈ 1.0101%` — not above
/// 100%, and not only for extreme rates. At the 1000% rate ceiling the
/// per-multiply is what binds, and it leaves `MAX/1000 ≈ 7.92e25`; the pair
/// would leave `MAX/11 ≈ 7.2e27`, looser by `1000/11 ≈ 91×`. Anyone quoting a
/// "largest safe net" for a rate must take the MINIMUM of the two, and must
/// remember that a per-LINE bound still says nothing about a per-DOCUMENT sum
/// (work unit T3).
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

    /// A rate SET: several linked taxes on one line, each with its own `id`,
    /// because the breakdown's one-entry-per-tax shape is part of what the
    /// document-total invariant pins.
    fn taxes(rates: &[&str]) -> Vec<Tax> {
        rates
            .iter()
            .enumerate()
            .map(|(index, rate)| Tax {
                id: index as i64 + 1,
                ..tax(rate)
            })
            .collect()
    }

    /// ONE contribution, written the way [`calculate_line_taxes`] writes it.
    /// Restating the definition is the point: the expected money below is built
    /// from the rule rather than typed in, so each literal beside it checks the
    /// rule's result instead of being a copy of it.
    fn contribution(net: Decimal, rate: Decimal) -> Decimal {
        round_to_cents(net * rate / percent())
    }

    /// The figure a PER-UNIT implementation would produce for the same line:
    /// the unit's tax rounded, added to the unit, and only then multiplied by
    /// the quantity.
    ///
    /// Decision 3 is a claim about WHERE the one `round_to_cents` happens, and a
    /// claim about a location is only checkable once the alternative is written
    /// down as arithmetic. This is built from the same `round_to_cents` the
    /// contract uses, so the two differ in exactly one respect — the quantity
    /// the rounding is applied to — and every counter-example below is about
    /// that one difference rather than about two rival rounding rules.
    fn per_unit_line_total(unit: Decimal, qty: Decimal, taxes: &[Tax]) -> Decimal {
        let unit_tax_total = taxes
            .iter()
            .fold(Decimal::ZERO, |sum, tax| sum + contribution(unit, tax.rate));
        round_to_cents(unit + unit_tax_total) * qty
    }

    /// The figure the cent is MEANT to be nearest: `net * (1 + SUM rate_i/100)`
    /// with nothing rounded anywhere.
    ///
    /// It is the yardstick both error statements below are measured against,
    /// and it is not a restatement of the contract — the contract rounds, this
    /// does not — which is what keeps the bound from being circular.
    fn exact_line_total(net: Decimal, taxes: &[Tax]) -> Decimal {
        let factor = taxes
            .iter()
            .fold(Decimal::ONE, |factor, tax| factor + tax.rate / percent());
        net * factor
    }

    /// The error bound, as a VALUE.
    ///
    /// [`round_to_cents`] moves its input by at most half a cent, and nothing
    /// in the contract rounds anything else, so `n` linked taxes bound the
    /// total at `n` half-cents. `extra_pins` pays for the OTHER pins the answer
    /// may pass through. It is zero whenever the net is already at
    /// [`MONEY_SCALE`] — every whole quantity of a two-decimal price — because
    /// a sum of values that are already at `MONEY_SCALE` is exact and the
    /// line's own pin is then a no-op. A net carrying MORE decimals (a
    /// fractional quantity, a three-decimal unit cost) goes through one real
    /// extra pin, and the bound pays for it rather than the test quietly
    /// excluding the case.
    fn error_bound(taxes: &[Tax], extra_pins: usize) -> Decimal {
        Decimal::from(taxes.len() + extra_pins) * cent() / Decimal::TWO
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

    /// DECISION 3, on the plan's own worked example, with BOTH answers asserted.
    ///
    /// Cost `0,03` at 21% in a quantity of 7 is the figure the decision was
    /// taken on, and it is here with the two numbers SEPARATED rather than
    /// described: the contract's answer, `0.25`, and the answer a per-unit
    /// implementation gives for the identical line, `0.28`, against an exact
    /// figure of `0.2541` that neither of them lands on. The two are asserted
    /// as numbers and asserted to DIFFER, which is what makes this a
    /// counter-example rather than a claim in a comment: an implementation that
    /// moved the rounding to the unit would answer `0.28` here and fail on the
    /// literal.
    ///
    /// The three fixtures are not chosen because they are awkward. The first is
    /// the plan's, the second reaches the same line net by a different route
    /// (so the line-level figure is fixed by the line, not by the unit, which is
    /// the whole claim), and the third multiplies the quantity by fourteen and
    /// leaves the line-level answer alone while the per-unit answer walks off by
    /// 37 cents — a hundred times the quantity would.
    #[test]
    fn the_line_rounds_once_at_the_line_and_not_at_the_unit() {
        for (raw_unit, rates, raw_qty, line, per_unit, exact) in [
            ("0.03", ["21"].as_slice(), "7", "0.25", "0.28", "0.2541"),
            ("0.07", ["21"].as_slice(), "3", "0.25", "0.24", "0.2541"),
            ("0.03", ["21"].as_slice(), "100", "3.63", "4.00", "3.63"),
        ] {
            let linked = taxes(rates);
            let (unit, qty) = (dec(raw_unit), dec(raw_qty));
            let label = format!("{raw_unit} at {rates:?} in {raw_qty}");

            // The line's net, from the contract's own definition of one.
            let net =
                line_net_amount(qty, unit).expect("a fixture amount is arithmetic this carries");
            let calc = calculate_line_taxes(net, &linked)
                .expect("an ordinary amount and rate set is arithmetic this contract carries");

            // The arithmetic that produces the literal, so the literal cannot
            // drift from the rule silently: one pinned contribution per tax,
            // their sum, and the net plus that sum.
            let built: Vec<Decimal> = linked.iter().map(|t| contribution(net, t.rate)).collect();
            assert_eq!(
                calc.taxes.iter().map(|s| s.amount).collect::<Vec<_>>(),
                built,
                "{label}: the breakdown is the pinned contributions"
            );
            assert_eq!(calc.tax_total, built.iter().sum::<Decimal>(), "{label}");
            assert_eq!(calc.total, dec(line), "{label}: the line-level figure");

            // The unrounded figure, so the two errors below are measured
            // against a yardstick rather than against each other.
            assert_eq!(exact_line_total(net, &linked), dec(exact), "{label}: exact");

            // The counter-example, as a number: the alternative's arithmetic is
            // run here and its result asserted, so a reader can check that the
            // two really are computed by the rules named and not by hand.
            let alternative = per_unit_line_total(unit, qty, &linked);
            assert_eq!(alternative, dec(per_unit), "{label}: the per-unit figure");
            assert_ne!(
                alternative, calc.total,
                "{label}: the per-unit figure {alternative} must differ from the line-level one \
                 {}, or the counter-example proves nothing",
                calc.total
            );
        }
    }

    /// THE PROPERTY, and it is a bound over a range rather than three fixtures.
    ///
    /// Decision 3's claim is that the line-level answer stays within half a cent
    /// of the exact figure REGARDLESS OF QUANTITY. Stated as arithmetic: a
    /// `round_to_cents` call moves its input by at most `cent() / 2`, the
    /// contract applies it once per linked tax and nowhere else, and a sum of
    /// values already at `MONEY_SCALE` is exact — so for `n` linked taxes on a
    /// two-decimal net the total is at most `n` half-cents from
    /// `net * (1 + SUM rate_i/100)`, and NO quantity appears in that bound at
    /// all. The per-unit alternative's error is `qty * delta` for a `delta`
    /// fixed by the unit cost and the rates alone, so it grows without limit as
    /// the quantity grows while this one cannot. That is the difference, and it
    /// is why the decision is not a preference.
    ///
    /// The second arm is the fractional quantity, and it is here because the
    /// bound above is FALSE without it rather than merely untested: a net with
    /// more than two decimals goes through one real extra pin — the line's own
    /// total — and pays one more half-cent. A test that swept only whole
    /// quantities of two-decimal prices would be claiming a bound the contract
    /// does not hold, and a mutation that moved the line's pin onto the
    /// contributions would slip past it.
    #[test]
    fn the_line_level_error_stays_within_half_a_cent_per_tax_at_every_quantity() {
        let units = [
            "0.01", "0.03", "0.07", "0.11", "0.13", "0.29", "1.37", "2.50", "99.99",
        ];
        let rate_sets: [&[&str]; 5] = [
            &["21"],
            &["10.5"],
            &["27"],
            &["21", "10.5"],
            &["21", "10.5", "3"],
        ];
        // Quantities an operator actually types, and a few past the end of the
        // range a purchase form offers, because the claim is REGARDLESS of
        // quantity and a bound nobody has pushed on is a guess.
        let quantities: [Decimal; 10] =
            ["1", "2", "3", "7", "12", "25", "50", "99", "100", "1000"].map(dec);

        for rates in rate_sets {
            let linked = taxes(rates);
            for raw_unit in units {
                let unit = dec(raw_unit);
                for qty in quantities {
                    let net = line_net_amount(qty, unit).expect("a fixture amount is carried");

                    let calc = calculate_line_taxes(net, &linked)
                        .expect("an ordinary amount and rate set is carried");
                    let error = (calc.total - exact_line_total(net, &linked)).abs();

                    // A whole quantity of a two-decimal price: the net is
                    // already at `MONEY_SCALE`, so the line's own pin has
                    // nothing to move and `n` half-cents is the whole bound.
                    assert!(
                        error <= error_bound(&linked, 0),
                        "{rates:?} on {raw_unit} x {qty}: the line total is {error} from exact, \
                         past the {} half-cent bound the single line-level rounding gives it",
                        linked.len()
                    );

                    // The same bound restated on the quarter that has to be
                    // there: a quarter of a cent is unreachable with any number
                    // of taxes, and its loss is what a move of the rounding to
                    // the unit costs. Reported as a counter-example rather than
                    // a claim, so the difference survives the mutation that
                    // makes it true.
                    let per_unit_error = (per_unit_line_total(unit, qty, &linked)
                        - exact_line_total(net, &linked))
                    .abs();
                    if per_unit_error > error_bound(&linked, 0) {
                        let per_unit_tax_total: Decimal = linked
                            .iter()
                            .fold(Decimal::ZERO, |sum, t| sum + contribution(unit, t.rate));
                        let delta = per_unit_tax_total
                            - linked
                                .iter()
                                .fold(Decimal::ZERO, |sum, t| sum + unit * t.rate / percent());
                        assert_eq!(
                            per_unit_error,
                            delta.abs() * qty,
                            "{rates:?} on {raw_unit} x {qty}: rounding the UNIT is a signed error \
                             of {delta} carried by all {qty} of them, and {delta} * {qty} is not \
                             the line-level answer"
                        );
                    }
                }
            }
        }

        // THE FRACTIONAL ARM. A net carrying more decimals than the currency
        // pays one more half-cent, for the line's own pin.
        for rates in rate_sets {
            let linked = taxes(rates);
            let bound = error_bound(&linked, 1);
            for raw_unit in ["0.005", "0.015", "0.333"] {
                let unit = dec(raw_unit);
                for raw_qty in ["0.5", "1.5", "2.25", "3.75"] {
                    let qty = dec(raw_qty);
                    let net = line_net_amount(qty, unit).expect("a fixture amount is carried");
                    let calc = calculate_line_taxes(net, &linked)
                        .expect("an ordinary amount and rate set is carried");
                    let error = (calc.total - exact_line_total(net, &linked)).abs();
                    assert!(
                        error <= bound,
                        "{rates:?} on {raw_unit} x {raw_qty}: the line total is {error} from exact, \
                         past the {bound} the single line-level rounding plus the line's own pin \
                         give it"
                    );
                }
            }
        }
    }

    /// THE DOCUMENT-TOTAL INVARIANT, and it is a format requirement rather than a
    /// choice: the Libro de IVA Digital rejects a document whose total differs
    /// from the sum of its components, and every amount it carries is thirteen
    /// integers and two decimals. There is no adjustment line to park a
    /// difference on, so the only way the format is satisfiable at all is if the
    /// stored breakdown already reconciles with the stored total.
    ///
    /// This is therefore also the test that kills a "round the document" design:
    /// accumulate the contributions at full precision, round the tax total once
    /// at the end and derive the total from THAT, and every fixture below lands
    /// on money that looks right and is unreportable. Each tax stays pinned here
    /// because the stored tax total is the operator's and the accountant's only
    /// view of how the gross was reached.
    ///
    /// The sweep spans one, two and three linked taxes — additive, never
    /// compounding, every tax on the same net — plus the untaxed line, and both
    /// an ordinary two-decimal net and a fractional one that the line's own pin
    /// has to close.
    #[test]
    fn the_line_total_is_the_net_plus_its_pinned_contributions_with_nothing_between() {
        let rate_sets: [&[&str]; 7] = [
            &[],
            &["0"],
            &["21"],
            &["0.5"],
            &["21", "10.5"],
            &["21", "10.5", "3"],
            &["10.5", "3", "27", "0.5"],
        ];
        for rates in rate_sets {
            let linked = taxes(rates);
            for raw_net in [
                "0.00", "0.01", "0.03", "0.07", "0.10", "0.21", "0.15", "0.005", "0.105", "1.37",
                "2.50", "99.99", "123.456",
            ] {
                let net = dec(raw_net);
                let calc = calculate_line_taxes(net, &linked)
                    .expect("an ordinary amount and rate set is carried");

                assert_eq!(
                    calc.net_subtotal, net,
                    "{rates:?} on {raw_net}: the net is the net"
                );
                assert_eq!(
                    calc.taxes.len(),
                    linked.len(),
                    "{rates:?} on {raw_net}: one entry per linked tax, and none invented"
                );

                // The components, rebuilt from the contract's own definition.
                let components: Vec<Decimal> =
                    linked.iter().map(|t| contribution(net, t.rate)).collect();
                assert_eq!(
                    calc.taxes.iter().map(|s| s.amount).collect::<Vec<_>>(),
                    components,
                    "{rates:?} on {raw_net}: each entry is its own pinned contribution"
                );
                assert_eq!(
                    calc.taxes.iter().map(|s| s.rate).collect::<Vec<_>>(),
                    linked.iter().map(|t| t.rate).collect::<Vec<_>>(),
                    "{rates:?} on {raw_net}: the rate beside the amount is the one applied"
                );

                // THE INVARIANT. Three ways of saying the same identity, because
                // each one is what a document-level round breaks in a different
                // place: the tax total is the sum of the entries, the total is
                // the net plus that sum, and the read side recomputes the same
                // figure from the same two stored numbers.
                let components_total = components.iter().sum::<Decimal>();
                assert_eq!(
                    calc.tax_total, components_total,
                    "{rates:?} on {raw_net}: the tax total is the sum of the breakdown"
                );
                assert_eq!(
                    calc.total,
                    round_to_cents(net + components_total),
                    "{rates:?} on {raw_net}: the total is the net plus the sum of the breakdown"
                );
                assert_eq!(
                    calc.total,
                    tax_inclusive_total(net, components_total),
                    "{rates:?} on {raw_net}: the write and the read side name the same figure"
                );
            }
        }
    }
}
