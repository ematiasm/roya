//! The PURE and TOTAL final-price solve: a typed tax-inclusive price in, a net
//! price and a markup out, or a typed refusal (final price markup, U1).
//!
//! # What "total" means here, precisely
//!
//! **Every** input either produces an answer or a typed refusal. There is no
//! third outcome: no panic, no `None`, no silently different price. Two
//! mechanisms here make that true:
//!
//! * [`max_solvable_final_price`] bounds the final price before any arithmetic
//!   touches it, because `validate_effective_prices` deliberately has no upper
//!   ceiling — whether a price is plausible is a product decision, not an
//!   arithmetic one — and an operator supplies this one.
//! * [`tax_arithmetic_fits`] dry-runs the tax contract in checked form over the
//!   window's extreme nets, so a rate set whose arithmetic cannot be carried is
//!   refused rather than multiplied. Every arithmetic step that could overflow
//!   inside the solve is checked, including building the window's own endpoints.
//!
//! # The tax contract is total NOW; this module's own guards stay anyway
//!
//! [`calculate_line_taxes`] used to multiply and add with rust_decimal's raw
//! operators, which PANIC on overflow, and this module was written to keep its
//! own callers away from it. The contract has since been made total in its own
//! right: it returns a typed [`PriceRefusal`] and every step in it is checked,
//! the final add included.
//!
//! The two mechanisms above are NOT removed, and the reason is not redundancy.
//! [`tax_arithmetic_fits`] decides WHICH refusal a caller of this solve sees:
//! the contract's own `TaxArithmeticTooLarge` names a document line's amount
//! and a tax total, while `TaxRateTooLargeToPrice` names the RATE against a
//! final price, which is the only vocabulary a person setting a final price
//! can act on. Deleting the dry run would silently repoint this solve's own
//! refusals at the line's vocabulary, and the ceiling would then be the only
//! thing standing between an operator and an answer the search cannot carry.
//!
//! Both mechanisms above are load-bearing and neither is redundant. For an
//! all-NON-NEGATIVE rate set the price bound alone is enough, because the
//! divisor is at least 1 and scales the net down at least as fast as the rate
//! scales the product up. The saving clause is that this is exactly the set the
//! product screen can produce, since `validate_rate` rejects negative rates — an
//! all-NEGATIVE set gets no such bound, because a divisor below one AMPLIFIES
//! the net instead of shrinking it.
//!
//! The case that needs the second mechanism is a MIXED-sign set, where the
//! cancellation hides in the sum: `1e20` against `-(1e20 - 0.01)` leaves the
//! divisor at 1.0001 and the enormous rate applied at full size, and no ceiling
//! on the price can prevent that overflow because the overflow does not depend
//! on the price.
//!
//! # Why this is a solve and not a division
//!
//! The pricing chain is one-directional everywhere else in this application:
//! cost and markup derive the net, and the net plus its taxes derive the final
//! price. That direction is a formula. This module walks it backwards, and the
//! walk is not invertible by arithmetic, because the tax contract rounds each
//! contribution on its own ([`calculate_line_taxes`]):
//!
//! ```text
//! final(net) = round2(net + SUM round2(net * rate_i / 100))
//! ```
//!
//! `final` is therefore a staircase, not a line, and it has flat steps: several
//! nets can share one final price, and some final prices are shared by no net at
//! all. Dividing the target by `1 + SUM rate_i / 100` places an estimate within
//! a cent or two of the answer and is wrong about the exact cent often enough to
//! be useless — an operator who types 100 and gets a net whose final price is
//! 100.01 has been told a lie about what they asked for. So the estimate only
//! PLACES a window; a bounded cent search inside that window finds a net whose
//! final price is exactly the typed value, or refuses.
//!
//! # What this module does NOT do
//!
//! It opens no connection, reads no clock and writes nothing. The net it solves
//! is the same canonical net every document line already snapshots, so setting
//! a final price is a one-shot conversion and not a pricing mode: changing a tax
//! later still moves the final price, which is the already-documented
//! consequence of the net being the truth.
//!
//! # The division this introduces
//!
//! [`derive_net_sale_price`] states in its own comment that a percentage shift
//! is a multiplication by `0.01` because "this project never divides a
//! `Decimal`". That is true of the MARKUP derivation, and it is the reason the
//! markup half of this solve inverts the formula and then VERIFIES the answer
//! through the real deriver instead of trusting its own arithmetic. It is not
//! true of the project as a whole: the tax contract has always computed
//! `net * rate / 100` ([`calculate_line_taxes`]). Inverting a gross is a
//! division by construction — there is no multiplication that lands on a
//! staircase from the wrong side — so this module performs the first division
//! used to DERIVE a stored price, deliberately, and says so here rather than
//! leaving the next reader to believe the comment above applies to the crate.
//!
//! # Reuse
//!
//! * [`calculate_line_taxes`] is the ONLY definition of a final price. This
//!   module never rounds a tax itself, so a solved net and a document line can
//!   never disagree by a cent about the same net.
//! * [`derive_net_sale_price`] is the ONLY definition of a markup-derived net.
//!   The markup this module returns has been round-tripped through it, so the
//!   stored markup and the stored net are the same pair the save path would
//!   produce.
//! * [`validate_effective_prices`] is the ONLY definition of a legal price. The
//!   typed target goes through it, so this solve cannot invent a state the save
//!   would refuse.
//!
//! Every item below is `pub` for U2 — the route and the ladder control that
//! consumes this solve — and this unit lands the arithmetic FIRST, because the
//! feature document splits them precisely because the arithmetic is the part
//! that can be silently wrong. Until U2 there is no production reader, and this
//! crate is a binary, where `pub` does not by itself exempt an item from the
//! dead-code pass. This is the same situation, and the same remedy, as
//! `PriceRefusal::ALL`, which carries the attribute for the same reason.
#![allow(dead_code)]

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};

use crate::models::{PriceRefusal, ProductKind, Tax};
use crate::services::inventory::{derive_net_sale_price, validate_effective_prices};
use crate::services::line_taxes::{
    calculate_line_taxes, round_to_cents, LineTaxSnapshot, MONEY_SCALE,
};

/// What a solve can fail with. There is no other failure mode: this module
/// returns a typed [`PriceRefusal`] or an answer, never a partial one and never
/// a silently different price.
pub type SolveResult<T> = Result<T, PriceRefusal>;

/// One hundred, the divisor that turns a percentage rate into a factor.
const PERCENT: Decimal = Decimal::ONE_HUNDRED;
/// Two, as a `Decimal`. Written out rather than imported so the arithmetic in
/// [`search_radius_cents`] reads as the fraction of a cent it is.
const TWO: Decimal = Decimal::TWO;
/// One hundredth, the money scale every cent-quantised candidate is built at.
fn cent() -> Decimal {
    Decimal::new(1, MONEY_SCALE)
}

/// Extra cents of window beyond the proven error bound.
///
/// The bound itself is tight — see [`search_radius_cents`] — so this is a
/// margin against an arithmetic detail of `Decimal`'s division, not a guess.
/// Widening it costs a few loop iterations and cannot change a correct answer,
/// because every candidate is checked against the tax contract before it can
/// win.
const SEARCH_MARGIN_CENTS: i64 = 1;

/// The hard cap on the window, in cents, and a TOTality guard rather than a
/// tuning knob.
///
/// The proven radius is `ceil((n + 1) / 2 / divisor) + 1` cents, which grows
/// without limit as the linked rates approach -100% in total: at a divisor of
/// `0.0001` it asks for a window of half a million cents, and a rate set like
/// that would turn a bounded search into a hang. Capping trades a possible
/// FALSE REFUSAL on a pathological rate set for a guaranteed termination, and
/// that is the right trade here: the module's rule is that an unreachable
/// target is refused and never approximated, so the worst case of hitting the
/// cap is a refusal an operator can act on, never a price nobody asked for.
const MAX_SEARCH_RADIUS_CENTS: i64 = 1_000;

/// The decimal places tried for the markup, coarsest first.
///
/// Zero is FIRST on purpose, not last. A whole-number markup is the one an
/// operator would type by hand and the one worth storing; extra decimals are
/// only worth their clutter when the round number does not reproduce the net.
/// The ladder is bounded and finite, and every rung is verified rather than
/// assumed — see [`solve_markup`].
const MARKUP_PRECISION_LADDER: [u32; 8] = [0, 2, 4, 8, 12, 16, 20, 24];

/// The largest final price this solve accepts, in whatever unit the operator
/// typed it in: [`max_solvable_final_price`]. Anything above it is refused with
/// [`PriceRefusal::FinalPriceTooLarge`].
///
/// # Why the solve needs a ceiling at all
///
/// [`calculate_line_taxes`] computes each contribution as `net * rate / 100`
/// and used to do it with rust_decimal's raw `*` operator, which PANICS on
/// overflow. It is total now — it returns a typed refusal — but this solve
/// keeps its own ceiling, and for two reasons that survive the contract being
/// fixed.
///
/// The first is the refusal's vocabulary: the contract would answer
/// `TaxArithmeticTooLarge` ("the line amount is too large to calculate its
/// taxes"), which names a document line. A person typing a final price needs to
/// be told the PRICE is out of range, and `FinalPriceTooLarge` is the sentence
/// that says so.
///
/// The second is that the ceiling is this solve's own promise, checked before
/// any arithmetic touches the number.
/// `validate_effective_prices` deliberately has no upper price bound (whether a
/// price is plausible is a product decision, not an arithmetic one), so without
/// a ceiling a 28-digit final price reaches the multiplication and is reported
/// as somebody else's problem. A refusal an operator can read, attributed to the
/// field they typed in, is the only acceptable answer to a number that does not
/// fit.
///
/// # Why this value
///
/// The bound has to satisfy two pulls at once, and 1e18 is where they stop
/// fighting:
///
/// * **It must be far below what the arithmetic can carry.** The binding
///   operation is `net * rate`, and the useful fact is not "the rate is
///   bounded" — it is not — but that for an all-NON-NEGATIVE rate set the two
///   cancel: the divisor `1 + Σ rate/100` is at least 1 and scales the net down
///   by at least the factor the rate scales the product up, so `net * rate`
///   stays within a factor of `100 * final_price` however enormous the rate is.
///   A rate of 1e20 against a final price of 1e18 yields a product of 1e20, not
///   1e38. Since `validate_rate` rejects negative rates, this covers every rate
///   the product screen can produce, and the product is bounded by about
///   `100 * 1e18 = 1e20` — nine decades below `Decimal`'s ~7.92e28. The markup
///   half needs less still: `net * 100` at 1e18 is 1e20.
///
///   The qualifier is NOT optional. An all-NEGATIVE rate set has a divisor BELOW
///   one, which AMPLIFIES the net rather than shrinking it, and the product is
///   then bounded by nothing at all. Such a set is not reachable from the
///   product screen, which is the only reason the bound is sufficient here.
/// * **It must be far above any real price.** The entire money supply of the
///   planet is on the order of 1e16 in any single currency, so 1e18 is a
///   quintillion in one unit: two orders of magnitude of headroom over
///   everything that has ever existed to trade, and no operator is anywhere
///   near being able to type a smaller bound that is still safe.
///
/// The mixed-sign rate set is the one hole that reasoning leaves, because the
/// cancellation can hide inside the SUM rather than inside one product: one
/// enormous rate against another that cancels it leaves the divisor near 1, so
/// the net stays near the full final price while the enormous rate is applied at
/// full size. No final-price bound closes that, because the overflow does not
/// depend on the price. It is covered by [`tax_arithmetic_fits`] instead, and the
/// two together are what make the totality claim in the module docs true.
///
/// A bound chosen by copying the largest value some other test happened to use
/// would be an accident with no argument behind it. This one is nine decades of
/// headroom on the arithmetic side and a hundredfold on the business side, and
/// both numbers are asserted in the tests below.
///
/// A function and not a `const` because rust_decimal exposes no const
/// constructor for a value this large; `Decimal::new` is not a `const fn`.
pub fn max_solvable_final_price() -> Decimal {
    Decimal::new(1_000_000_000_000_000_000, 0)
}

/// The markup half of a solved answer.
///
/// The two variants are the answer to "did the markup follow?", stated by the
/// type rather than by a comment a caller can miss: a caller that matches
/// `Verified` knows it may store `markup_pct`, and a caller that matches
/// `NotDerivableWithoutCost` knows it must NOT invent one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SolvedMarkup {
    /// A markup that the REAL deriver reproduced the solved net from.
    Verified {
        /// The markup to store, already round-tripped through
        /// [`derive_net_sale_price`].
        markup_pct: Decimal,
        /// Which rung of [`MARKUP_PRECISION_LADDER`] closed the round trip.
        /// `Some` in the answer rather than in a log, because a markup stored
        /// with 24 decimals and one stored with none are the same price and very
        /// different numbers to read.
        decimal_places: u32,
    },
    /// The product has no positive cost, so there is nothing for a markup to be
    /// a percentage OF. No markup is invented and `markup_pct` stays absent; the
    /// caller states the markup is not derivable without a cost.
    ///
    /// This is a SUCCESS, not a refusal: the net was solved and the final price
    /// is honoured. It is its own variant because a caller that treats a
    /// missing markup as a failed solve would invent a 0% markup, which is a
    /// different product.
    NotDerivableWithoutCost,
}

/// A solved final price: the typed target, the net that reproduces it exactly,
/// and the markup that reproduces the net.
#[derive(Debug, Clone, PartialEq)]
pub struct FinalPriceSolve {
    /// The typed target, echoed exactly as received. The answer's own claim:
    /// this is the number the caller asked for, and every field below exists to
    /// justify it.
    pub final_price: Decimal,
    /// The net that re-derives `final_price` EXACTLY through
    /// [`calculate_line_taxes`]. This is the value to store as `sale_price`.
    pub net_price: Decimal,
    /// The tax money that separates the net from the final price, at
    /// `MONEY_SCALE`, from the shared tax contract.
    pub tax_total: Decimal,
    /// The per-tax breakdown behind `tax_total`, in the order the taxes were
    /// resolved. Taken from the shared contract, so the rows a caller shows
    /// reconcile with the total by construction.
    pub breakdown: Vec<LineTaxSnapshot>,
    /// The markup that re-derives `net_price` from the cost, or the typed
    /// statement that no markup exists.
    pub markup: SolvedMarkup,
}

/// Turn a typed tax-inclusive price into the net price that produces it exactly
/// and the markup that produces the net.
///
/// `final_price` is what the customer pays. `taxes` are the tax definitions
/// already resolved for the product, in the same order and with the same
/// activity filtering the ladder and the document lines use, because the answer
/// is only the same answer if it is computed from the same taxes. `kind`,
/// `cost_price` and `stored_markup_pct` are the product's own values, read
/// only to decide what may be stored — this function writes nothing.
///
/// # The rules, in the order they apply
///
/// 1. A product that already carries a markup and has no positive cost is
///    refused with [`PriceRefusal::MarkupNeedsPositiveCost`], the save path's
///    own refusal. Writing a new net beside that markup would leave the row
///    contradicting itself, and the next save would silently undo the change.
/// 2. Linked rates that add up to -100% or less cannot be inverted and are
///    refused with [`PriceRefusal::FinalPriceNotInvertible`]. There is no
///    estimate to place a window around, and the rate set has to change first.
/// 3. The typed target goes through [`validate_effective_prices`], the one
///    definition of a legal price. A negative final price is refused here for
///    the same reason a negative net is: it is not a price, whatever taxes
///    hang off it. (A negative target is genuinely reachable from a negative net
///    when a rate is negative, which is exactly why this cannot be left to the
///    search to notice.)
/// 4. The net is searched for. No net means
///    [`PriceRefusal::FinalPriceUnreachable`], never a nearby price.
/// 5. The markup is solved and verified. A product with no positive cost gets
///    [`SolvedMarkup::NotDerivableWithoutCost`]; anything that cannot close
///    within [`MARKUP_PRECISION_LADDER`] is refused rather than stored.
pub fn solve_final_price(
    final_price: Decimal,
    kind: ProductKind,
    cost_price: Decimal,
    stored_markup_pct: Option<Decimal>,
    taxes: &[Tax],
) -> SolveResult<FinalPriceSolve> {
    // -- rule 0: the price is inside what the tax arithmetic can carry --------
    // FIRST, before anything reads a rate or multiplies. The tax contract panics
    // on overflow and this solve supplies a number an operator typed, so the
    // ceiling belongs here. See `max_solvable_final_price` for the value and the
    // argument behind it, and for what this does NOT cover about the contract.
    if final_price > max_solvable_final_price() {
        return Err(PriceRefusal::FinalPriceTooLarge);
    }

    // -- rule 1: the product's own stored state must not be contradicted ------
    if stored_markup_pct.is_some() && cost_price <= Decimal::ZERO {
        return Err(PriceRefusal::MarkupNeedsPositiveCost);
    }

    // -- rule 2: an uninvertible rate set has no estimate ---------------------
    // Read here, on the TYPED RATE SET and before the target is looked at, so
    // the refusal names the rates rather than blaming a price that was never the
    // problem — and handed to the search so the factor is derived once.
    let divisor = gross_divisor(taxes).ok_or(PriceRefusal::FinalPriceNotInvertible)?;

    // -- rule 3: the typed price is a legal price ------------------------------
    validate_effective_prices(kind, final_price, cost_price)?;

    // -- rule 4: the net ------------------------------------------------------
    let net = solve_net(final_price, taxes, divisor)?;

    // -- rule 5: the markup ---------------------------------------------------
    // A product with no positive cost has nothing for a markup to be a
    // percentage OF, so none is invented. `SolvedMarkup` says so in its type,
    // and the net above stands on its own: the final price is still honoured.
    let markup = if cost_price <= Decimal::ZERO {
        SolvedMarkup::NotDerivableWithoutCost
    } else {
        solve_markup(cost_price, net)?
    };

    // The breakdown is recomputed from the SOLVED net, never carried over from
    // a candidate, so the rows published are the rows that justify the answer.
    //
    // `?` rather than a panic: the contract is total and refuses, and a refusal
    // that reaches this solve is the same fact `tax_arithmetic_fits` states with
    // the SOLVE's own variant. Carried through unchanged, so the caller still
    // learns that a rate set cannot carry this price.
    let calculation =
        calculate_line_taxes(net, taxes).map_err(|_| PriceRefusal::TaxRateTooLargeToPrice)?;
    Ok(FinalPriceSolve {
        final_price,
        net_price: net,
        tax_total: calculation.tax_total,
        breakdown: calculation.taxes,
        markup,
    })
}

/// The net whose final price is EXACTLY `final_price`.
///
/// # The window
///
/// The estimate `final_price / divisor` is the centre; every candidate is a
/// whole number of cents away from it, and the half-width is
/// [`search_radius_cents`] — a proven bound, not a tuning parameter. The
/// window is only a place to LOOK: nothing is believed because it is in range.
/// Every candidate is priced through [`calculate_line_taxes`], the one tax
/// contract, and only a candidate whose final price equals the typed value can
/// win.
///
/// # The tie-break, and why it is not the loop order
///
/// More than one net can share a final price, because each tax contribution is
/// rounded on its own and the resulting staircase has flat steps. The window
/// then holds several correct answers, and something has to choose between them.
///
/// **The rule: the candidate CLOSEST TO THE UNROUNDED ESTIMATE wins.** The
/// estimate is the operator's intent — "this should sell at about 0.10
/// including tax" — and the flat step is an artefact of rounding that nobody
/// chose. The nearest net is the one that best represents what was asked for.
///
/// Loop order is NOT that rule. Scanning the window upwards answers the lowest
/// candidate and scanning it downwards answers the highest, so "whichever the
/// loop found first" would make the stored price depend on the direction a
/// `for` loop happens to run in — the same target, two products, two prices.
/// That is why the candidates are scored and compared here rather than returned
/// from inside the loop.
///
/// **Ties in the distance** — the estimate sitting exactly between two
/// candidates — are broken toward the SMALLER net, because the distance is
/// compared strictly and the window is walked from its low end, so the first
/// candidate at the winning distance is the smaller one.
///
/// That secondary rule is a DETERMINISM rule and nothing more. It is not a
/// commercial preference, and it is worth being blunt about the direction: the
/// net is stored as `sale_price`, so the smaller net is LESS revenue before tax
/// on every sale. It is chosen because the arithmetic genuinely cannot
/// distinguish the two candidates — the operator's estimate sat exactly between
/// them — and a rule that is at least fixed and explainable is worth more than
/// an arbitrary one. If a future change makes the estimate land between two
/// nets often enough to matter commercially, the right fix is a decision about
/// pricing, recorded here, not an accident of iteration order.
///
/// **This rule is currently unreachable through the product screen.**
/// `TaxService::validate_rate` rejects a negative rate, and with every rate
/// non-negative each contribution is monotone in the net, so no two nets ever
/// share a final price and the tie-break never fires. The rule exists because
/// this function is total: it takes resolved `Tax` values, not a validated form,
/// and a rate set that grosses BELOW the net must get an answer that does not
/// depend on which direction a loop ran in.
fn solve_net(final_price: Decimal, taxes: &[Tax], divisor: Decimal) -> SolveResult<Decimal> {
    // The one deliberate division in a derivation: inverting a gross has no
    // multiplication, and the module docs say why this is the first one here.
    let estimate = final_price
        .checked_div(divisor)
        .ok_or(PriceRefusal::FinalPriceUnreachable)?;

    let base = round_to_cents(estimate);
    let radius = search_radius_cents(taxes.len(), divisor);

    // PROVE the window is carryable before any candidate enters the tax
    // contract. `|net * rate|` grows with `|net|`, so the two extreme nets of a
    // window centred on `base` bound every candidate between them, and checking
    // those two is enough. See `tax_arithmetic_fits` for why this has to happen
    // here rather than being assumed.
    //
    // CHECKED, and that is the whole point of these two lines. `Decimal` panics
    // on addition overflow, so building an endpoint with `base + step` could
    // crash the request from inside the guard meant to stop one. It is
    // reachable: a rate set that grosses the price down to a factor of 1e-11
    // divides a final price that is INSIDE the bound straight past
    // `Decimal::MAX`, and `base` lands on the ceiling.
    //
    // `cent() * Decimal::from(radius)` needs no checked form: `radius` is capped
    // at `MAX_SEARCH_RADIUS_CENTS`, so the product is at most 10.00.
    let step = cent() * Decimal::from(radius);
    let low = base
        .checked_sub(step)
        .ok_or(PriceRefusal::NetPriceTooLarge)?;
    let high = base
        .checked_add(step)
        .ok_or(PriceRefusal::NetPriceTooLarge)?;
    if !tax_arithmetic_fits(low, taxes) || !tax_arithmetic_fits(high, taxes) {
        return Err(PriceRefusal::TaxRateTooLargeToPrice);
    }

    let mut best: Option<(Decimal, Decimal)> = None;

    for offset in -radius..=radius {
        // Checked for the same reason as the endpoints, and kept checked even
        // though a representable pair of endpoints makes every interior point
        // representable: a total function should not depend on that inference
        // holding, and the call costs one comparison per candidate.
        let candidate = base
            .checked_add(cent() * Decimal::from(offset))
            .ok_or(PriceRefusal::NetPriceTooLarge)?;
        // The two endpoints were proved carryable above, so a refusal here is
        // the same fact stated by the contract itself. It is reported with the
        // solve's own variant for the same reason the guard reports it that
        // way: a machine reading a solve's refusal needs the solve's vocabulary.
        let Ok(calculation) = calculate_line_taxes(candidate, taxes) else {
            return Err(PriceRefusal::TaxRateTooLargeToPrice);
        };
        if calculation.total != final_price {
            continue;
        }
        let distance = (candidate - estimate).abs();
        match best {
            // Strictly closer only: an equal distance keeps the candidate found
            // first, and the window is walked from the low end, so that is the
            // smaller net. See the tie-break note above.
            Some((best_distance, _)) if best_distance <= distance => {}
            _ => best = Some((distance, candidate)),
        }
    }

    best.map(|(_, net)| net)
        .ok_or(PriceRefusal::FinalPriceUnreachable)
}

/// Whether the shared tax contract can carry `net` against this rate set without
/// overflowing — a faithful dry run of [`calculate_line_taxes`] with every
/// operation in its checked form.
///
/// The contract is total now and would answer `Err` on its own, so the honest
/// reason this exists is the REFUSAL it chooses, not the crash it prevents. A
/// caller of this solve must hear `TaxRateTooLargeToPrice` — "a linked rate is
/// too large to price this final price" — because that is the one field they
/// can change. Letting the contract's own answer through would name a line
/// amount and a tax total, which are not what the person typing a final price
/// typed. A pre-check that reports the same fact in the solve's own vocabulary
/// is worth four comparisons per window.
///
/// It is also cheaper, and it is checked BEFORE the window is walked: a rate
/// set that cannot carry the search must be refused without paying for a
/// search to discover it.
///
/// # Why the price bound does not make this redundant
///
/// [`max_solvable_final_price`] already covers every all-NON-NEGATIVE rate set,
/// and not by a small margin: the divisor is at least 1 and scales the net down
/// at least as fast as the rate scales the product up, so `net * rate` stays
/// near `100 * final_price` however large the rate gets. A single rate of 1e20 is
/// safe for exactly that reason.
///
/// The qualifier carries the weight, and it is not a formality. An all-NEGATIVE
/// rate set has a divisor BELOW one, so the divisor AMPLIFIES the net and the
/// product is bounded by nothing at all — the bound is simply the wrong tool
/// there, and it is `validate_rate` rejecting negative rates that keeps that
/// case off every path the product screen can reach.
///
/// The remaining hole is a MIXED-sign set, where the cancellation hides in the
/// sum rather than in one product: `1e20` against `-(1e20 - 0.01)` sums to
/// `+0.01`, the divisor stays at 1.0001, the net stays near the whole final
/// price, and the enormous rate is applied at full size. The overflow there does
/// not depend on the price at all, so no ceiling on the price can prevent it —
/// only looking.
///
/// The steps mirror the contract one for one — multiply, divide by 100, round to
/// cents, accumulate, add the net — so a `true` here means the real call cannot
/// overflow and a `false` means it would have.
fn tax_arithmetic_fits(net: Decimal, taxes: &[Tax]) -> bool {
    let mut tax_total = Decimal::ZERO;
    for tax in taxes {
        let Some(raw) = net.checked_mul(tax.rate) else {
            return false;
        };
        let Some(scaled) = raw.checked_div(PERCENT) else {
            return false;
        };
        let Some(running) = tax_total.checked_add(round_to_cents(scaled)) else {
            return false;
        };
        tax_total = running;
    }
    net.checked_add(tax_total).is_some()
}

/// The markup that makes [`derive_net_sale_price`] produce `net` from `cost`.
///
/// # Why the answer is verified rather than derived
///
/// Inverting the deriver's own formula gives `markup = net * 100 / cost - 100`,
/// and in exact arithmetic that reproduces the net exactly. It does not survive
/// contact with a stored value: `markup_pct` is a rounded `Decimal` an operator
/// will read, and the deriver rounds its OWN output to cents. A cost of 3 with a
/// net of 10 needs 233.333...%; store the whole number 233 and the next save
/// derives 9.99 — the product silently moves a cent the moment it is touched
/// again. So this function does not trust its arithmetic: every candidate
/// markup is run back through the REAL deriver and compared, and only a
/// candidate that reproduces the net is returned.
///
/// # The bound
///
/// [`MARKUP_PRECISION_LADDER`] is finite and starts at zero decimals, because a
/// whole-number markup is the one worth storing. Extra decimals are earned: each
/// rung narrows the price error a rounded markup can introduce, and the ladder
/// stops at the first rung that closes. If no rung closes, the solve REFUSES. It
/// never returns an unverified markup, because an unverified markup is a promise
/// the next save will break.
///
/// The deriver's own refusals are carried out unchanged when it is the thing
/// that refused — a markup at or below -100, or no positive cost, are its rules
/// and this module has no opinion that outranks them. When the deriver refuses
/// some rungs and the next one closes, the refusal is not the answer: see the
/// note at the `Err` arm below.
fn solve_markup(cost_price: Decimal, net: Decimal) -> SolveResult<SolvedMarkup> {
    // The deriver's formula, inverted. Every step is checked because the
    // operands are unbounded: `net * 100` alone can leave the 28-digit range,
    // and there is then no representable markup at all.
    let exact = net
        .checked_mul(PERCENT)
        .and_then(|scaled| scaled.checked_div(cost_price))
        .and_then(|quotient| quotient.checked_sub(PERCENT))
        .ok_or(PriceRefusal::FinalPriceMarkupUnreachable)?;

    let mut deriver_refusal: Option<PriceRefusal> = None;
    for decimal_places in MARKUP_PRECISION_LADDER {
        let markup_pct =
            exact.round_dp_with_strategy(decimal_places, RoundingStrategy::MidpointAwayFromZero);
        match derive_net_sale_price(cost_price, Some(markup_pct), Decimal::ZERO) {
            // The comparison is the whole contract: the deriver's output IS the
            // stored net, or this candidate is not the answer.
            Ok(derived) if derived == net => {
                return Ok(SolvedMarkup::Verified {
                    markup_pct,
                    decimal_places,
                });
            }
            Ok(_) => {}
            Err(refusal) => {
                // Remembered, NOT returned, and that is the load-bearing part.
                // A coarse rung can round a markup that is legally above the
                // deriver's -100 boundary ONTO it: cost 100000000 with a net of
                // 0.01 needs -99.99999999%, and 0, 2 and 4 decimals all round
                // to -100 and are refused while 8 decimals is fine. A refusal
                // is evidence about the RUNG, not about the price, so the
                // ladder keeps climbing and this is only the reason to report if
                // nothing ever closes.
                deriver_refusal.get_or_insert(refusal);
            }
        }
    }

    // The exhausted-ladder backstop, and it is reached in practice by only one
    // of the two ways to get here.
    //
    // `Some(refusal)` is the ordinary case: the deriver refused every rung, and
    // the deriver's own reason is the accurate one to report — a markup at or
    // below -100 is ITS rule and this module has no opinion that outranks it.
    //
    // `None` means every rung was legal and none reproduced the net, which is
    // the `FinalPriceMarkupUnreachable` case. It is a BACKSTOP, not an expected
    // outcome, and the reason is arithmetic rather than empirical: the finest
    // rung is 24 decimal places, so for any cost below 1e24 the rounded markup
    // sits within `0.5 / cost` of the exact quotient — many decades inside the
    // half-cent window the deriver rounds to — and the quotient itself is exact
    // whenever `net * 100 / cost` fits in 28 digits, which it does for every
    // cent-quantised net and any cost above `net * 100 / 7.92e28`.
    // `final_price_the_markup_ladder_always_closes_for_a_cent_quantised_net`
    // sweeps that grid and shows no pair ever exhausts the ladder. The arm stays
    // because a total function should have an answer for "I have no better
    // reason", and a total function must not be the thing that panics.
    Err(deriver_refusal.unwrap_or(PriceRefusal::FinalPriceMarkupUnreachable))
}

/// `1 + SUM rate_i / 100`: the factor that turns a net into its exact,
/// UNROUNDED gross. `None` when the rates gross the net away entirely, which
/// is the one rate set with no inverse at all.
///
/// Deliberately additive and never compounding, because that is what
/// [`calculate_line_taxes`] does: every rate applies to the same net.
fn gross_divisor(taxes: &[Tax]) -> Option<Decimal> {
    let rate_sum = taxes
        .iter()
        .try_fold(Decimal::ZERO, |sum, tax| sum.checked_add(tax.rate))?;
    let divisor = Decimal::ONE.checked_add(rate_sum.checked_div(PERCENT)?)?;
    (divisor > Decimal::ZERO).then_some(divisor)
}

/// The window half-width, in cents, that is PROVABLY wide enough to contain the
/// answer.
///
/// The proof: `final(net) = round2(net + SUM round2(net * rate_i / 100))`, and
/// each of the `n + 1` roundings — one per tax plus the total — moves the exact
/// value `net * divisor` by at most half a cent. So
/// `|final - net * divisor| <= (n + 1) / 2` cents, and therefore
/// `|final / divisor - net| <= (n + 1) / (2 * divisor)` cents, which is what
/// this returns (rounded up, plus [`SEARCH_MARGIN_CENTS`]).
///
/// The divisor is in the formula rather than assumed away on purpose: for an
/// all-NON-NEGATIVE rate set the divisor is at least 1 and the window is two or
/// three cents wide, but a set that grosses BELOW the net pushes the true answer
/// further from the estimate, and a window sized for the common case would miss
/// it. "Grosses up" means every rate is non-negative, so `divisor >= 1`; it is
/// NOT a claim about the divisor's value, because a mixed-sign rate set can sum
/// above zero and still need a wide window.
///
/// The claim is a test, not a note: `final_price_the_window_is_sufficient_over_
/// six_hundred_of_net` walks every net from 0.00 to 600.00 for every rate set
/// this suite uses and fails if the computed window ever holds no net that
/// reproduces the target.
///
/// [`MAX_SEARCH_RADIUS_CENTS`] then caps the result, for the reason documented
/// there: a refusal is recoverable and a hang is not.
fn search_radius_cents(tax_count: usize, divisor: Decimal) -> i64 {
    let bound_cents = Decimal::from(tax_count as i64 + 1)
        .checked_div(TWO)
        .and_then(|half| half.checked_div(divisor))
        .and_then(|exact| exact.ceil().to_i64())
        .unwrap_or(MAX_SEARCH_RADIUS_CENTS);
    bound_cents
        .saturating_add(SEARCH_MARGIN_CENTS)
        .min(MAX_SEARCH_RADIUS_CENTS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn dec(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    /// A resolved `Tax` value. The solve takes resolved tax definitions, so a
    /// test builds one without touching the database — the same shape the tax
    /// snapshot tests use for the same reason.
    fn tax(id: i64, code: &str, rate: &str) -> Tax {
        let midnight = chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        Tax {
            id,
            code: code.to_string(),
            name: format!("Tax {code}"),
            rate: dec(rate),
            is_active: true,
            created_by: 0,
            updated_by: None,
            created_at: midnight,
            updated_at: midnight,
        }
    }

    /// A generated rate set as the borrowed slice the solve takes, so a test can
    /// build one with a repetition count instead of writing it out.
    fn as_strs(rates: &[String]) -> Vec<&str> {
        rates.iter().map(String::as_str).collect()
    }

    /// The linked taxes of a rate set, in the order they are written.
    fn taxes(rates: &[&str]) -> Vec<Tax> {
        rates
            .iter()
            .enumerate()
            .map(|(index, rate)| tax(index as i64 + 1, &format!("T{index}"), rate))
            .collect()
    }

    /// The whole solve as one call, for the cases that are about the ANSWER.
    fn solve(
        final_price: &str,
        cost_price: &str,
        stored_markup_pct: Option<&str>,
        rates: &[&str],
    ) -> SolveResult<FinalPriceSolve> {
        solve_for_kind(
            final_price,
            ProductKind::Product,
            cost_price,
            stored_markup_pct,
            rates,
        )
    }

    /// The same, for the cases where the product KIND is the point.
    fn solve_for_kind(
        final_price: &str,
        kind: ProductKind,
        cost_price: &str,
        stored_markup_pct: Option<&str>,
        rates: &[&str],
    ) -> SolveResult<FinalPriceSolve> {
        solve_final_price(
            dec(final_price),
            kind,
            dec(cost_price),
            stored_markup_pct.map(dec),
            &taxes(rates),
        )
    }

    /// The refusal a solve returned. Every refusal test reads through this, so a
    /// solve that quietly starts SUCCEEDING fails with the net it invented
    /// rather than with a comparison the answer type cannot support.
    fn refusal(result: SolveResult<FinalPriceSolve>) -> PriceRefusal {
        match result {
            Ok(answer) => panic!("expected a refusal, got the net {}", answer.net_price),
            Err(err) => err,
        }
    }

    /// What the naive approach does: divide the target by the gross factor and
    /// round. In the test suite rather than in the solve, because the whole
    /// point of the search is that this is not the answer — every test that
    /// compares against it is a test about a real, verified disagreement.
    fn naive_division(final_price: &str, rates: &[&str]) -> Decimal {
        let divisor = gross_divisor(&taxes(rates)).unwrap();
        round_to_cents(dec(final_price) / divisor)
    }

    /// The final price of a net through the ONE tax contract, so no test in
    /// this module can agree with a second, private idea of the equation.
    ///
    /// The contract is total, so a refusal here is a real answer and not a
    /// test artefact: these fixtures are ordinary amounts with non-negative
    /// rate sets, and a refusal would mean the equation under test cannot be
    /// carried at all.
    fn final_of(net: &str, rates: &[&str]) -> Decimal {
        calculate_line_taxes(dec(net), &taxes(rates))
            .expect("an ordinary net and a non-negative rate set are carried")
            .total
    }

    /// Every net in `lo..=hi` (in cents) whose final price is exactly `target`,
    /// found WITHOUT the solve's window. This is the independent oracle the
    /// cross-checks compare against: it cannot be fooled by a wrong window,
    /// because it does not use one.
    fn every_net_with_final(target: &str, rates: &[&str], hi_cents: i64) -> Vec<Decimal> {
        (0..=hi_cents)
            .map(|cents| Decimal::new(cents, MONEY_SCALE))
            .filter(|net| final_of(&net.to_string(), rates) == dec(target))
            .collect()
    }

    // -----------------------------------------------------------------------
    // The three shapes of tax set the ladder actually shows
    // -----------------------------------------------------------------------

    /// No linked taxes: the final price IS the net, and the breakdown says so
    /// with no rows rather than with a zero row.
    #[test]
    fn final_price_with_no_taxes_solves_to_the_typed_value() {
        let answer = solve("100.00", "5", None, &[]).unwrap();

        assert_eq!(answer.final_price, dec("100.00"));
        assert_eq!(answer.net_price, dec("100.00"));
        assert_eq!(answer.tax_total, dec("0"));
        assert!(answer.breakdown.is_empty(), "no tax, no breakdown row");
        assert_eq!(final_of("100.00", &[]), dec("100.00"));
    }

    /// One linked tax. The contribution the solve publishes is the SAME
    /// contribution the tax contract produced, and the two add up to the typed
    /// value exactly.
    #[test]
    fn final_price_with_one_tax_solves_to_the_typed_value() {
        let answer = solve("100.00", "5", None, &["10"]).unwrap();

        assert_eq!(answer.net_price, dec("90.91"));
        assert_eq!(answer.tax_total, dec("9.09"));
        assert_eq!(answer.breakdown.len(), 1);
        assert_eq!(answer.breakdown[0].rate, dec("10"));
        assert_eq!(answer.breakdown[0].amount, dec("9.09"));
        assert_eq!(
            final_of(&answer.net_price.to_string(), &["10"]),
            dec("100.00"),
            "the solved net must re-derive to the typed value EXACTLY, not to a nearby one"
        );
    }

    /// Several linked taxes are ADDITIVE, never compounding, and the breakdown
    /// reconciles with the total the operator is shown.
    #[test]
    fn final_price_with_several_additive_taxes_solves_to_the_typed_value() {
        let rates = ["21", "10", "5"];
        let answer = solve("100.00", "5", None, &rates).unwrap();

        assert_eq!(answer.net_price, dec("73.53"));
        assert_eq!(answer.breakdown.len(), 3);
        // 73.53 * 21% = 15.4413 -> 15.44; * 10% = 7.353 -> 7.35; * 5% =
        // 3.6765 -> 3.68. Additive: 26.47, never 73.53 * 1.21 * 1.10 * 1.05.
        let amounts: Vec<Decimal> = answer.breakdown.iter().map(|row| row.amount).collect();
        assert_eq!(amounts, vec![dec("15.44"), dec("7.35"), dec("3.68")]);
        assert_eq!(answer.tax_total, dec("26.47"));
        assert_eq!(answer.net_price + answer.tax_total, dec("100.00"));
        assert_eq!(
            final_of(&answer.net_price.to_string(), &rates),
            dec("100.00")
        );
    }

    // -----------------------------------------------------------------------
    // THE CASES A DIVISION ALONE GETS WRONG
    // -----------------------------------------------------------------------

    /// The pinned fixture, found by exhaustively comparing the division against
    /// the equation over nets 0.00 to 300.00: with 10% and 5% linked, a typed
    /// `100.00` divides to `86.96`, and `86.96` grosses to `100.01`. The
    /// operator would have been told their product sells at 100.01.
    ///
    /// The disagreement is one cent and it is not a rounding artefact of the
    /// test: `86.96` is a real net whose real final price is `100.01`.
    #[test]
    fn final_price_where_the_naive_division_misses_by_a_cent() {
        let rates = ["10", "5"];

        let naive = naive_division("100.00", &rates);
        assert_eq!(naive, dec("86.96"), "the fixture: division lands on 86.96");
        assert_eq!(
            final_of(&naive.to_string(), &rates),
            dec("100.01"),
            "and 86.96 grosses to 100.01, so the division's answer is a price \
             the operator did not ask for"
        );

        let answer = solve("100.00", "5", None, &rates).unwrap();
        assert_eq!(
            answer.net_price,
            dec("86.95"),
            "the search must land on the net that grosses to exactly 100.00"
        );
        assert_eq!(
            final_of(&answer.net_price.to_string(), &rates),
            dec("100.00")
        );
        assert_ne!(
            answer.net_price, naive,
            "the two answers differ, which is the entire reason this is a search"
        );
    }

    /// The same class of miss on a second rate set and on a small target, so
    /// the first fixture cannot be dismissed as one lucky pair.
    #[test]
    fn final_price_where_the_division_misses_on_more_than_one_rate_set() {
        // The division's answer, the price it actually grosses to, and the net
        // that grosses to exactly what was typed. The middle column is pinned
        // per case because the division misses in BOTH directions: a rate set
        // with two taxes can leave the division a cent high or a cent low.
        for (target, rates, division_net, division_gross, exact_net) in [
            ("100.00", ["21", "5"].as_slice(), "79.37", "100.01", "79.36"),
            ("0.23", ["21", "10"].as_slice(), "0.18", "0.24", "0.17"),
            ("0.08", ["21", "5"].as_slice(), "0.06", "0.07", "0.07"),
        ] {
            assert_eq!(
                naive_division(target, rates),
                dec(division_net),
                "{target} with {rates:?}: the division's answer"
            );
            assert_eq!(
                final_of(division_net, rates),
                dec(division_gross),
                "{target} with {rates:?}: the price the division's net really sells at"
            );
            assert_ne!(
                dec(division_gross),
                dec(target),
                "{target} with {rates:?}: and it is not the price that was typed"
            );
            let answer = solve(target, "5", None, rates).unwrap();
            assert_eq!(
                answer.net_price,
                dec(exact_net),
                "{target} with {rates:?}: the searched net"
            );
            assert_eq!(final_of(exact_net, rates), dec(target));
        }
    }

    /// THE GUARD PROVES NOTHING.
    ///
    /// The obvious cheap guard is "is the division's answer within a cent of the
    /// target?". It passes on the fixture above — `100.01` is within one cent of
    /// `100.00` — and the answer is still wrong, because the operator asked for
    /// a price that grosses to exactly what they typed. A tolerance guard can
    /// only ever return a price that is off by up to the tolerance, and this
    /// feature's whole claim is that it is not.
    ///
    /// This test exists to fail loudly if someone replaces the exact search with
    /// a tolerance check: it asserts that the tolerance is satisfied by the
    /// wrong net and NOT by the right one.
    #[test]
    fn final_price_a_within_a_cent_tolerance_guard_would_prove_nothing() {
        let rates = ["10", "5"];

        let wrong = naive_division("100.00", &rates);
        let right = solve("100.00", "5", None, &rates).unwrap().net_price;

        // The guard a lazy implementation would write, satisfied by the wrong
        // net...
        assert!(
            (final_of(&wrong.to_string(), &rates) - dec("100.00")).abs() <= cent(),
            "the tolerance guard passes on the WRONG net, so passing it proves nothing"
        );
        // ...and it cannot tell the two apart, because it is the same number
        // for both.
        assert_ne!(wrong, right, "and the two nets are genuinely different");
        assert_eq!(
            (final_of(&wrong.to_string(), &rates) - dec("100.00")).abs() <= cent(),
            (final_of(&right.to_string(), &rates) - dec("100.00")).abs() <= cent(),
            "the guard accepts both, so it cannot be the thing that decides"
        );
    }

    // -----------------------------------------------------------------------
    // The tie-break: proximity to the estimate, never loop order
    // -----------------------------------------------------------------------

    /// More than one net can produce the same final price, because the tax
    /// contract's per-contribution rounding makes the final price a staircase
    /// with flat steps. When that happens the answer is the net CLOSEST TO THE
    /// UNROUNDED ESTIMATE, because the estimate is the operator's intent
    /// ("this is about 0.10 including tax") and the flat step is an artefact of
    /// rounding, not a choice anybody made.
    ///
    /// The fixture is chosen so that BOTH loop orders are wrong: scanning the
    /// window upwards would answer 0.09, scanning it downwards 0.11, and only
    /// the distance to the estimate answers 0.10. An implementation that
    /// returned "whichever the loop found first" would pass this test only by
    /// accident.
    #[test]
    fn final_price_ties_are_resolved_by_proximity_to_the_estimate() {
        let rates = ["-60", "-10"];
        let divisor = gross_divisor(&taxes(&rates)).unwrap();
        let estimate = dec("0.03") / divisor;
        assert_eq!(estimate, dec("0.10"), "the fixture: the estimate is 0.10");

        let candidates = every_net_with_final("0.03", &rates, 50);
        assert_eq!(
            candidates,
            vec![dec("0.09"), dec("0.10"), dec("0.11")],
            "the fixture: three nets share this final price"
        );

        let answer = solve("0.03", "5", None, &rates).unwrap();
        assert_eq!(answer.net_price, dec("0.10"), "the tie-break, not the loop");
        assert_ne!(
            answer.net_price,
            *candidates.iter().min().expect("candidates are non-empty"),
            "an upward scan would have answered the lowest candidate"
        );
        assert_ne!(
            answer.net_price,
            *candidates.iter().max().expect("candidates are non-empty"),
            "a downward scan would have answered the highest candidate"
        );
    }

    /// The tie-break is proximity, not "the middle one" and not "the biggest
    /// one". Here the estimate falls between the second and third candidate and
    /// the third is nearer, so a "pick the middle" or "pick the largest net"
    /// implementation would answer 0.09 or 0.11 and be wrong.
    #[test]
    fn final_price_a_tie_picks_the_candidate_nearest_the_estimate_not_the_extreme() {
        let rates = ["-79"];
        let divisor = gross_divisor(&taxes(&rates)).unwrap();
        let estimate = dec("0.02") / divisor;
        let candidates = every_net_with_final("0.02", &rates, 50);
        assert_eq!(
            candidates,
            vec![dec("0.08"), dec("0.09"), dec("0.10"), dec("0.11")],
            "the fixture: four nets share this final price"
        );

        let answer = solve("0.02", "5", None, &rates).unwrap();
        let nearest = candidates
            .iter()
            .min_by(|left, right| {
                let left = (*left - estimate).abs();
                let right = (*right - estimate).abs();
                left.partial_cmp(&right).expect("finite distances")
            })
            .copied()
            .expect("candidates are non-empty");
        assert_eq!(answer.net_price, nearest, "the answer IS the nearest");
        assert_ne!(answer.net_price, dec("0.08"), "and not the lowest");
        assert_ne!(answer.net_price, dec("0.11"), "and not the highest");
    }

    /// With EVERY rate non-negative, no two nets share a final price, so the
    /// tie-break never has to fire. This is the state the application is
    /// actually in, and the tie tests above exist because this solve is total
    /// and must answer a rate set that is not like this one.
    ///
    /// The condition that buys injectivity is "every rate is non-negative", NOT
    /// "the divisor is above one" — which is what the tie-break note in
    /// `solve_net` used to claim, and which the next test refutes.
    #[test]
    fn final_price_the_tie_break_is_never_needed_when_every_rate_is_non_negative() {
        for rates in [
            vec!["21"],
            vec!["10"],
            vec!["5"],
            vec!["100"],
            vec!["21", "10"],
            vec!["21", "10", "5"],
        ] {
            for cents in 1..=400i64 {
                let net = Decimal::new(cents, MONEY_SCALE);
                let target = final_of(&net.to_string(), &rates);
                let answer = solve(&target.to_string(), "5", None, &rates).unwrap();
                assert_eq!(answer.net_price, net, "the net is its own answer");
            }
        }
    }

    /// THE COUNTEREXAMPLE to "a divisor above one means no ties".
    ///
    /// Rates of -5% and 10% sum to +5, so the divisor is 1.05 — comfortably
    /// above one — and yet two nets share a final price: 0.09 and 0.10 both
    /// gross to 0.10. The positive rate's contribution grows by 0.001 per cent
    /// of net and the negative one's shrinks by 0.001, and the two cancel.
    ///
    /// This test exists so the reason in `solve_net` cannot be quietly
    /// "simplified" back to a divisor test by a future maintainer who reads it
    /// as equivalent. It is not equivalent, and this is the difference.
    #[test]
    fn final_price_a_divisor_above_one_does_not_rule_out_a_tie() {
        let rates = ["-5", "10"];
        let divisor = gross_divisor(&taxes(&rates)).unwrap();
        assert!(
            divisor > Decimal::ONE,
            "the fixture: the divisor IS above one, {divisor}"
        );

        let candidates = every_net_with_final("0.10", &rates, 50);
        assert_eq!(
            candidates,
            vec![dec("0.09"), dec("0.10")],
            "the fixture: a divisor above one, and still two nets for one final price"
        );

        let answer = solve("0.10", "5", None, &rates).unwrap();
        assert_eq!(
            answer.net_price,
            dec("0.10"),
            "the estimate is 0.095238..., and 0.10 is the nearer candidate"
        );
    }

    // -----------------------------------------------------------------------
    // The markup half, and the verified round trip
    // -----------------------------------------------------------------------

    /// A markup is not derived here; it is VERIFIED, and a whole number is
    /// preferred when one closes.
    ///
    /// Cost 5 with a solved net of 100 needs `5 * (1 + m/100) = 100`, so
    /// `m = 1900` — a whole number, so the coarsest rung of the ladder closes
    /// and no decimals are stored. The witness is the REAL deriver, not this
    /// module's arithmetic, which is the whole point: the stored pair is the
    /// pair the save path would produce.
    #[test]
    fn final_price_the_markup_is_round_tripped_through_the_real_deriver() {
        let answer = solve("100.00", "5", None, &[]).unwrap();
        let SolvedMarkup::Verified {
            markup_pct,
            decimal_places,
        } = answer.markup
        else {
            panic!("a positive cost must produce a verified markup");
        };

        assert_eq!(markup_pct, dec("1900"), "5 * (1 + 1900/100) = 100");
        assert_eq!(decimal_places, 0, "no decimals were needed");
        assert_eq!(
            derive_net_sale_price(dec("5"), Some(markup_pct), Decimal::ZERO).unwrap(),
            answer.net_price,
            "the real deriver, not this module's arithmetic, is the witness"
        );
    }

    /// THE PRECISION REFINEMENT, and it is not hypothetical.
    ///
    /// Cost 3 with a solved net of 10 needs a markup of 233.333...%. The
    /// obvious whole-number guess, 233, derives 9.99 — a cent SHORT. A solve
    /// that assumed its own arithmetic would store 233 and the next save would
    /// quietly move the product to 9.99. The verification catches it and the
    /// ladder refines: 233.33 derives 10.00 exactly.
    #[test]
    fn final_price_the_markup_refines_precision_when_the_round_number_misses() {
        // Rates 10% + 5% on a net of 10.00 give a final price of 11.50, so the
        // target below solves to exactly this net.
        let answer = solve("11.50", "3", None, &["10", "5"]).unwrap();
        assert_eq!(answer.net_price, dec("10.00"));

        assert_eq!(
            answer.markup,
            SolvedMarkup::Verified {
                markup_pct: dec("233.33"),
                decimal_places: 2,
            },
            "233 gives 9.99, so the ladder had to refine to two decimals"
        );
        // The refinement was NECESSARY, not decorative: the coarse guess is
        // still wrong, and the test says so out loud.
        assert_eq!(
            derive_net_sale_price(dec("3"), Some(dec("233")), Decimal::ZERO).unwrap(),
            dec("9.99"),
            "the whole-number markup the ladder rejected"
        );
        assert_eq!(
            derive_net_sale_price(dec("3"), Some(dec("233.33")), Decimal::ZERO).unwrap(),
            dec("10.00"),
            "and the one it accepted"
        );
    }

    /// The refinement also has to recover from a guess that overshoots, not
    /// only from one that undershoots: cost 7 with a net of 10 needs 42.857...%
    /// and 43 derives 10.01, a cent OVER.
    #[test]
    fn final_price_the_markup_refines_when_the_round_number_overshoots() {
        let answer = solve("11.50", "7", None, &["10", "5"]).unwrap();
        assert_eq!(answer.net_price, dec("10.00"));

        assert_eq!(
            answer.markup,
            SolvedMarkup::Verified {
                markup_pct: dec("42.86"),
                decimal_places: 2,
            }
        );
        assert_eq!(
            derive_net_sale_price(dec("7"), Some(dec("43")), Decimal::ZERO).unwrap(),
            dec("10.01"),
            "43 overshoots by a cent, in the other direction"
        );
        assert_eq!(
            derive_net_sale_price(dec("7"), Some(dec("42.86")), Decimal::ZERO).unwrap(),
            dec("10.00")
        );
    }

    /// THE LADDER MUST NOT STOP AT THE DERIVER'S OWN REFUSAL.
    ///
    /// A cost of 100000000 with a solved net of 0.01 needs a markup of
    /// -99.99999999%. At zero decimals that rounds to exactly -100, and the
    /// deriver REFUSES -100 (`MarkupNotAboveMinus100`) — at two and at four
    /// decimals too. The exact markup is above the boundary, so a finer rung is
    /// legal and reproduces the net.
    ///
    /// The plausible implementation is to treat a deriver refusal as final: it
    /// refuses the product, and the solve reports the refusal. That is wrong
    /// here, and it is wrong in the direction that loses a sale the operator
    /// could have made. A refusal on a COARSE rung is evidence about that rung,
    /// not about the price.
    #[test]
    fn final_price_the_markup_ladder_refines_past_a_deriver_refusal() {
        let answer = solve("0.01", "100000000", None, &[]).unwrap();
        assert_eq!(answer.net_price, dec("0.01"));

        // The coarse rungs are genuinely refused, not merely wrong.
        for coarse in ["-100", "-100.00", "-100.0000"] {
            assert_eq!(
                derive_net_sale_price(dec("100000000"), Some(dec(coarse)), Decimal::ZERO),
                Err(PriceRefusal::MarkupNotAboveMinus100),
                "{coarse} is on the boundary the deriver refuses"
            );
        }
        // And a fine enough rung closes.
        assert_eq!(
            answer.markup,
            SolvedMarkup::Verified {
                markup_pct: dec("-99.99999999"),
                decimal_places: 8,
            },
            "the ladder had to climb past three refusals to reach a legal markup"
        );
        assert_eq!(
            derive_net_sale_price(dec("100000000"), Some(dec("-99.99999999")), Decimal::ZERO)
                .unwrap(),
            dec("0.01")
        );
    }

    // -----------------------------------------------------------------------
    // The solve must be TOTAL IN FACT, not only in prose
    // -----------------------------------------------------------------------

    /// THE PANIC, as it was. `calculate_line_taxes` computed `net * rate` with
    /// the raw `Decimal` operator, which panics on overflow. With a 21% rate
    /// linked, a 7e28 final price made `7e28 * 21` leave the representable range
    /// and took the whole request down.
    ///
    /// The contract now refuses instead of panicking, so this test passes for a
    /// NEW reason and that is exactly why it stays: it must keep answering with
    /// the SOLVE's own variants, so the day someone deletes the dry run or the
    /// ceiling this fails on the variant rather than on a crash. A green solve
    /// must not depend on the contract below it happening to be total.
    ///
    /// Both verifier-confirmed inputs are here, and the sibling of the old
    /// largest test — which was safe only because it carried no tax at all.
    #[test]
    fn final_price_a_final_price_that_overflows_the_tax_arithmetic_is_refused_not_a_panic() {
        for (target, rates) in [
            // `Decimal::MAX` with a 100% rate: MAX * 100 overflows.
            ("79228162514264337593543950335", ["100"].as_slice()),
            // 7e28 with a 21% rate: 7e28 * 21 overflows.
            ("70000000000000000000000000000", ["21"].as_slice()),
            // The sibling the old suite was missing: the same order of magnitude
            // as the largest value it already tested, but WITH a linked tax.
            ("10000000000000000000000000", ["21"].as_slice()),
        ] {
            assert_eq!(
                refusal(solve(target, "5", None, rates)),
                PriceRefusal::FinalPriceTooLarge,
                "{target} with {rates:?} must be refused, not panic"
            );
        }
    }

    /// The bound is a BOUND, not a blanket rejection of large prices: the
    /// largest price the solve accepts is still solved, and solved exactly.
    /// Otherwise the guard above would be indistinguishable from "large prices
    /// are refused", which is a different and much worse rule.
    #[test]
    fn final_price_the_largest_price_the_solve_accepts_is_still_solved() {
        let largest = max_solvable_final_price().to_string();
        let answer = solve(&largest, "5", None, &[]).unwrap();
        assert_eq!(answer.final_price, max_solvable_final_price());
        assert_eq!(answer.net_price, max_solvable_final_price());
        assert_eq!(
            final_of(&answer.net_price.to_string(), &[]),
            max_solvable_final_price()
        );

        // And one cent over the bound is refused, so the boundary is where it is
        // documented to be.
        let over = (max_solvable_final_price() + cent()).to_string();
        assert_eq!(
            refusal(solve(&over, "5", None, &[])),
            PriceRefusal::FinalPriceTooLarge
        );
    }

    /// The bound on the PRICE is not enough on its own, and this is the case
    /// that proves it — a case that took real work to find, because the obvious
    /// construction does not work.
    ///
    /// A single enormous NON-NEGATIVE rate CANNOT overflow: the divisor is at
    /// least 1 and scales the net down at least as fast as the rate scales the
    /// product up, so `net * rate` stays near `100 * final_price` no matter how
    /// big the rate is. A rate of 1e20 at a final price of 1e18 gives a product
    /// of 1e20, not 1e38. So the price bound alone already covers every
    /// all-non-negative rate set — which is every rate the product screen can
    /// produce, because `validate_rate` rejects negatives. An all-NEGATIVE set
    /// would get no such bound at all, since a divisor below one amplifies the
    /// net instead of shrinking it.
    ///
    /// What is left is a MIXED-sign set: one enormous rate with a second rate
    /// cancelling it. The sum stays near zero, so the divisor stays near 1 and
    /// the net stays near the final price, while the enormous rate is applied at
    /// full size. `validate_rate` puts no ceiling, so such a set is
    /// representable, and the solve has to notice before the tax contract
    /// multiplies rather than after.
    #[test]
    fn final_price_a_mixed_sign_rate_set_too_large_to_price_is_refused_not_a_panic() {
        // 1e20 against -(1e20 - 0.01): the sum is +0.01, so the divisor is
        // 1.0001, the net stays at ~1e18, and 1e18 * 1e20 leaves the range.
        let enormous = "100000000000000000000";
        let cancelling = "-99999999999999999999.99";
        assert_eq!(
            refusal(solve(
                "1000000000000000000",
                "5",
                None,
                &[enormous, cancelling]
            )),
            PriceRefusal::TaxRateTooLargeToPrice
        );

        // The most REACHABLE form of the same defect, and the reason the
        // pre-flight is not optional: a rate set of `Decimal::MAX` against
        // `-Decimal::MAX` sums to exactly zero, so the divisor is 1.0000, the
        // net stays at the full final price, and `net * MAX` overflows for any
        // net above about 1. An ordinary final price of 1.00 is enough to take
        // the request down, which no bound on the PRICE could have prevented.
        assert_eq!(
            refusal(solve(
                "1.00",
                "5",
                None,
                &[
                    "79228162514264337593543950335",
                    "-79228162514264337593543950335"
                ]
            )),
            PriceRefusal::TaxRateTooLargeToPrice,
            "a rate set that sums to zero must not multiply a net of 1.00 by \
             Decimal::MAX"
        );

        // One decade down on the enormous rate the very same construction is
        // inside the arithmetic: 1e18 * 1e10 is 1e28, still under the ~7.92e28
        // limit, and the solve answers. So the refusal is about the rate's
        // magnitude and not about the size of the price or about mixed signs
        // as such.
        let rates = ["10000000000", "-9999999999.99"];
        let control = solve("1000000000000000000", "5", None, &rates)
            .unwrap_or_else(|err| panic!("1e10 against -(1e10 - 0.01) was refused: {err:?}"));
        assert_eq!(
            final_of(&control.net_price.to_string(), &rates),
            dec("1000000000000000000"),
            "and the net it answered is a real solution of the same equation"
        );
    }

    /// A markup that cannot be REPRESENTED is refused, never stored, and the
    /// bound does not make that state unreachable — it moves it.
    ///
    /// The reachable construction is a cost small enough that inverting the
    /// deriver leaves the 28-digit range: `9e17 * 100 / 1e-18` is 9e37. The net
    /// is ordinary money, the cost is a vanishing fraction of a cent, and there
    /// is no `Decimal` markup_pct that could bridge them.
    #[test]
    fn final_price_a_markup_that_cannot_be_represented_is_refused() {
        assert_eq!(
            refusal(solve(
                "900000000000000000",
                "0.000000000000000001",
                None,
                &[]
            )),
            PriceRefusal::FinalPriceMarkupUnreachable,
            "9e17 * 100 / 1e-18 is 9e37, past Decimal's 28 digits, so no \
             markup_pct exists that derives this net from this cost"
        );
        // And the old construction is now refused for a DIFFERENT and more
        // honest reason: it is above the price bound, so the price is turned
        // away before the markup half is ever reached.
        assert_eq!(
            refusal(solve("10000000000000000000000000", "0.01", None, &[])),
            PriceRefusal::FinalPriceTooLarge,
            "1e25 is now out of range as a PRICE, which is the earlier and more \
             accurate refusal"
        );
    }

    /// The ladder's backstop is a BACKSTOP, and this is the evidence.
    ///
    /// The finest rung is 24 decimal places, so a cost below 1e24 leaves the
    /// rounded markup within `0.5 / cost` of the exact quotient — far inside the
    /// half-cent window the deriver rounds to. Sweeping a wide grid of cent
    /// costs and cent nets, the ladder always closes at some rung, and the
    /// `unwrap_or` arm in `solve_markup` is never the reason a solve fails.
    /// Anything above the bound is refused earlier, by rule 0.
    #[test]
    fn final_price_the_markup_ladder_always_closes_for_a_cent_quantised_net() {
        let mut closed_at: std::collections::BTreeMap<u32, usize> = Default::default();
        for cost in [
            "0.01", "0.03", "0.07", "0.33", "0.99", "1", "3.33", "9.99", "100",
        ] {
            for cents in 1..=1_500i64 {
                let net = Decimal::new(cents, MONEY_SCALE);
                let answer = solve(&net.to_string(), cost, None, &[])
                    .unwrap_or_else(|err| panic!("{net} from cost {cost} was refused: {err:?}"));
                let SolvedMarkup::Verified {
                    markup_pct,
                    decimal_places,
                } = answer.markup
                else {
                    panic!("{net} from cost {cost} produced no markup");
                };
                assert_eq!(
                    derive_net_sale_price(dec(cost), Some(markup_pct), Decimal::ZERO).unwrap(),
                    net,
                    "cost {cost} and markup {markup_pct} must re-derive {net}"
                );
                *closed_at.entry(decimal_places).or_default() += 1;
            }
        }
        assert!(
            closed_at.contains_key(&0),
            "the coarsest rung must still earn its place: {closed_at:?}"
        );
        assert!(
            closed_at.keys().copied().max().expect("a non-empty map") <= 8,
            "no cost under 1e24 should need more than 8 decimals: {closed_at:?}"
        );
    }

    /// THE GUARD ITSELF MUST NOT PANIC.
    ///
    /// The window endpoints were built with the raw `+`/`-` operators, and
    /// `Decimal` panics on addition overflow — so the guard introduced to
    /// prevent a panic could produce one.
    ///
    /// It takes a rate set that grosses the price almost to nothing. Exactly 100
    /// taxes at `-0.99999999999` sum to `-99.999999999`, so the divisor is
    /// `1e-11`, and a final price of `Decimal::MAX * 1e-11` — comfortably
    /// INSIDE the 1e18 bound, so the price guard passes — divides straight back
    /// to `Decimal::MAX`. `base` lands on the ceiling and adding the window's
    /// step to it overflows.
    ///
    /// The low endpoint does not rescue it: `base - step` is representable, the
    /// dry run on it succeeds, and the `||` reaches the high endpoint anyway.
    /// Only checked arithmetic at both sites closes this.
    #[test]
    fn final_price_a_window_that_cannot_be_built_is_refused_not_a_panic() {
        let many = |n: usize| -> Vec<String> {
            std::iter::repeat("-0.99999999999".to_string())
                .take(n)
                .collect()
        };

        // The exact shape that reaches the window: the divisor is positive and
        // tiny, so the solve gets as far as building the endpoints.
        let rates = many(100);
        let linked = taxes(&as_strs(&rates));
        let divisor = gross_divisor(&linked).expect("a divisor of 1e-11 still inverts");
        assert_eq!(
            divisor,
            Decimal::new(1, 11),
            "the fixture: 100 such taxes gross the price down by exactly 1e-11"
        );
        let estimate = dec("792281625142643375.93543950335")
            .checked_div(divisor)
            .expect("the repro's estimate divides");
        assert_eq!(
            estimate,
            Decimal::MAX,
            "and dividing the typed price by 1e-11 lands on Decimal::MAX exactly"
        );
        assert_eq!(
            round_to_cents(estimate),
            Decimal::MAX,
            "which survives rounding to cents, so `base + step` is the overflow"
        );

        assert_eq!(
            refusal(solve(
                "792281625142643375.93543950335",
                "5",
                None,
                &as_strs(&rates)
            )),
            PriceRefusal::NetPriceTooLarge,
            "the window cannot be built, so the solve must refuse rather than              overflow while guarding against an overflow"
        );

        let fewer = many(50);

        // One more tax and the divisor is no longer positive, so the rate set is
        // refused by rule 2 and never reaches the window at all. Pinned because
        // it is the boundary of the case above, and because "150 also panicked"
        // would be the wrong way to describe it.
        for n in [150usize, 200] {
            assert_eq!(
                refusal(solve(
                    "792281625142643375.93543950335",
                    "5",
                    None,
                    &as_strs(&many(n))
                )),
                PriceRefusal::FinalPriceNotInvertible,
                "{n} such taxes sum below -100, so the divisor is not positive"
            );
        }

        // And fewer taxes leave plenty of room: the estimate is nowhere near the
        // ceiling, so this is a boundary and not a blanket refusal. (The target
        // itself carries more decimals than a cent-quantised net can reproduce, so
        // the answer here is a gap in the staircase — what matters is that it is
        // NOT a window failure.)
        assert_ne!(
            refusal(solve(
                "792281625142643375.93543950335",
                "5",
                None,
                &as_strs(&fewer)
            )),
            PriceRefusal::NetPriceTooLarge,
            "50 such taxes halve the net, which is comfortably representable, so \\
             the window is built and the solve gets past it"
        );
    }

    // -----------------------------------------------------------------------
    // Typed refusals: every state the solve cannot honour
    // -----------------------------------------------------------------------

    /// Per-contribution rounding leaves gaps, and a typed value inside a gap is
    /// the tax contract's arithmetic, not a bug. With 21% linked, no net grosses
    /// to 0.03: 0.02 grosses to 0.02 and 0.03 grosses to 0.04.
    #[test]
    fn final_price_no_net_produces_this_target_is_refused() {
        assert_eq!(
            refusal(solve("0.03", "5", None, &["21"])),
            PriceRefusal::FinalPriceUnreachable,
            "0.02 -> 0.02 and 0.03 -> 0.04, so 0.03 is a gap in the staircase"
        );
        assert_eq!(
            refusal(solve("0.10", "5", None, &["5"])),
            PriceRefusal::FinalPriceUnreachable,
            "5% has the same shape of gap"
        );
        // And the refusal is specific: the neighbours ARE reachable, so this is
        // not a search that failed to find anything at all.
        assert_eq!(
            solve("0.02", "5", None, &["21"]).unwrap().net_price,
            dec("0.02")
        );
        assert_eq!(
            solve("0.04", "5", None, &["21"]).unwrap().net_price,
            dec("0.03")
        );
    }

    /// A target BELOW the smallest gross any net can reach. With a 100% rate
    /// the smallest reachable final price is 0.02, so 0.01 is refused — while
    /// 0.02 is solved normally, which is what makes this a floor and not a
    /// blanket rejection of small prices.
    #[test]
    fn final_price_below_the_smallest_reachable_gross_is_refused() {
        assert_eq!(
            refusal(solve("0.01", "5", None, &["100"])),
            PriceRefusal::FinalPriceUnreachable,
            "with a 100% rate the smallest reachable gross is 0.02"
        );
        assert_eq!(
            solve("0.02", "5", None, &["100"]).unwrap().net_price,
            dec("0.01"),
            "so 0.02 is reachable, from the smallest net there is"
        );
    }

    /// A negative final price is refused by the price rules, with the SAME
    /// sentences the save path uses — this solve cannot invent a price state
    /// the save would reject. It has to be the rules and not the search because
    /// a negative target IS arithmetically reachable from a negative net when a
    /// rate is negative, which is why leaving it to the search would be a
    /// silent pass.
    #[test]
    fn final_price_a_negative_target_is_refused() {
        assert_eq!(
            refusal(solve_for_kind(
                "-1.00",
                ProductKind::Product,
                "5",
                None,
                &[]
            )),
            PriceRefusal::SalePriceNotPositiveForProduct
        );
        assert_eq!(
            refusal(solve_for_kind(
                "-1.00",
                ProductKind::Service,
                "5",
                None,
                &[]
            )),
            PriceRefusal::SalePriceNegative,
            "a service has its own sentence for a negative price, so the rules \
             differ and the identity differs"
        );
        // The arithmetic really does reach a negative final price, which is
        // exactly why the rule above is load-bearing.
        assert_eq!(final_of("-0.91", &["10"]), dec("-1.00"));
    }

    /// A product price of zero is refused for a product and accepted for a
    /// service, which is the pre-existing divergence, surfaced here rather than
    /// re-decided.
    #[test]
    fn final_price_a_zero_target_follows_the_existing_price_rules() {
        assert_eq!(
            refusal(solve("0.00", "5", None, &[])),
            PriceRefusal::SalePriceNotPositiveForProduct
        );
        // A free service may take a free final price; the net is 0.00 and the
        // markup is then refused by the DERIVER, not invented.
        assert_eq!(
            refusal(solve_for_kind(
                "0.00",
                ProductKind::Service,
                "0.01",
                None,
                &[]
            )),
            PriceRefusal::MarkupNotAboveMinus100,
            "0 from a positive cost is a markup of exactly -100, which the \
             existing deriver already refuses"
        );
    }

    /// A rate set that grosses the net away entirely has no inverse, and the
    /// refusal is about the RATE SET, not about the target: no target is
    /// solvable until the rates change.
    #[test]
    fn final_price_a_rate_set_that_cannot_be_inverted_is_refused() {
        assert_eq!(
            refusal(solve("100.00", "5", None, &["-100"])),
            PriceRefusal::FinalPriceNotInvertible
        );
        assert_eq!(
            refusal(solve("100.00", "5", None, &["-60", "-40"])),
            PriceRefusal::FinalPriceNotInvertible,
            "additive, so -60 and -40 together are also -100"
        );
        // One step above the boundary it inverts again, and the same solve
        // answers normally. A -99% rate grosses at 1% of the net, so the net
        // behind a typed 100.00 is 10000.00 — and with a hundred candidate
        // nets sharing that final price, the tie-break is what puts it there.
        assert_eq!(
            solve("100.00", "5", None, &["-99"]).unwrap().net_price,
            dec("10000.00")
        );
    }

    // -----------------------------------------------------------------------
    // Zero cost: a final price is honoured, a markup is not invented
    // -----------------------------------------------------------------------

    /// A manual product with no cost takes a final price. The net is solved
    /// exactly as it would be with a cost, and the answer says IN ITS TYPE that
    /// no markup exists, so a caller cannot mistake "no markup" for "failed".
    #[test]
    fn final_price_zero_cost_takes_the_price_without_inventing_a_markup() {
        let rates = ["10", "5"];
        let answer = solve("100.00", "0", None, &rates).unwrap();

        assert_eq!(answer.net_price, dec("86.95"));
        assert_eq!(
            final_of(&answer.net_price.to_string(), &rates),
            dec("100.00")
        );
        assert_eq!(
            answer.markup,
            SolvedMarkup::NotDerivableWithoutCost,
            "the type says the markup is not derivable; it is not a missing \
             value and not a zero markup"
        );
    }

    /// The other zero-cost case: the product ALREADY carries a markup, so a new
    /// net beside it would contradict the row and the next save would silently
    /// undo the change. The save path's own refusal, reused verbatim.
    #[test]
    fn final_price_zero_cost_with_an_existing_markup_is_refused() {
        assert_eq!(
            refusal(solve("100.00", "0", Some("50"), &[])),
            PriceRefusal::MarkupNeedsPositiveCost
        );
        // Refused BEFORE the target is even looked at, so an unreachable target
        // does not change the reason: the stored state is the blocker.
        assert_eq!(
            refusal(solve("0.03", "0", Some("50"), &["21"])),
            PriceRefusal::MarkupNeedsPositiveCost
        );
        // And the same cost with NO markup is fine, which is what makes this
        // about the contradiction rather than about the cost.
        assert!(solve("100.00", "0", None, &[]).is_ok());
    }

    // -----------------------------------------------------------------------
    // Idempotence
    // -----------------------------------------------------------------------

    /// Solving the same final price twice gives the same net AND the same
    /// markup. The conversion is idempotent, so a re-run of the control, a
    /// retried request or a preview-then-write cannot move the product.
    #[test]
    fn final_price_solving_the_same_target_twice_is_idempotent() {
        for rates in [
            vec![],
            vec!["21"],
            vec!["21", "10", "5"],
            vec!["10", "5"],
            vec!["-60", "-10"],
        ] {
            for cents in [2i64, 1_150, 10_000, 100_000] {
                // The target is a REACHABLE one by construction — taken from a
                // net's own final price — because a target inside a rounding
                // gap is refused, and "twice" has to mean "twice on the same
                // question", not "twice on a different one".
                let target = final_of(&Decimal::new(cents, MONEY_SCALE).to_string(), &rates);
                let first = solve(&target.to_string(), "3", None, &rates)
                    .unwrap_or_else(|err| panic!("{target} with {rates:?} was refused: {err:?}"));
                let second = solve(&target.to_string(), "3", None, &rates)
                    .unwrap_or_else(|err| panic!("{target} with {rates:?} was refused: {err:?}"));
                assert_eq!(
                    first.net_price, second.net_price,
                    "{target} with {rates:?}: the net moved between two solves"
                );
                assert_eq!(
                    first.markup, second.markup,
                    "{target} with {rates:?}: the markup moved between two solves"
                );
                assert_eq!(
                    first, second,
                    "{target} with {rates:?}: the whole answer, breakdown included"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // The brute-force cross-check: independent evidence, not a plausible answer
    // -----------------------------------------------------------------------

    /// THE WINDOW IS SUFFICIENT, over the range the module docs claim.
    ///
    /// `search_radius_cents` is a proven bound, and this is the test that
    /// produces that claim: for every net in 0.00 to 600.00, and for every rate
    /// set the suite uses, the window the solve actually computes CONTAINS a net
    /// whose final price is exactly that net's own final price. A radius that
    /// were one cent too small would miss at some net in this range, and the
    /// search would report a target that is perfectly reachable as unreachable.
    ///
    /// The claim in the module docs is therefore evidence in this file, not a
    /// number from a scratch script.
    #[test]
    fn final_price_the_window_is_sufficient_over_six_hundred_of_net() {
        let rate_sets: [Vec<&str>; 10] = [
            vec![],
            vec!["21"],
            vec!["10"],
            vec!["5"],
            vec!["100"],
            vec!["0.5"],
            vec!["21", "10"],
            vec!["10", "5"],
            vec!["21", "10", "5"],
            vec!["21", "10", "5", "2.5"],
        ];

        for rates in rate_sets {
            let taxes = taxes(&rates);
            let divisor = gross_divisor(&taxes).expect("a non-negative rate set inverts");
            let radius = search_radius_cents(taxes.len(), divisor);

            for cents in 0..=60_000i64 {
                let net = Decimal::new(cents, MONEY_SCALE);
                let target = calculate_line_taxes(net, &taxes)
                    .expect("a non-negative rate set carries an ordinary net")
                    .total;
                let base = round_to_cents(target / divisor);

                let in_window = (-radius..=radius).any(|offset| {
                    calculate_line_taxes(base + cent() * Decimal::from(offset), &taxes)
                        .expect("a non-negative rate set carries an ordinary net")
                        .total
                        == target
                });
                assert!(
                    in_window,
                    "{net} with {rates:?} grosses to {target} and the window \
                     {base} +/- {radius} cents holds no net that reproduces it: \
                     the radius is too small"
                );
            }
        }
    }

    /// A wide sweep: every net in a long range is asked for ITS OWN final
    /// price, and the solve has to hand back a net that re-derives to exactly
    /// that value. The sweep is the point — a search that merely converged on
    /// something plausible would still have to be right for 20 000 targets per
    /// rate set, and the assertion is checked against the tax contract, not
    /// against the solve's own word.
    #[test]
    fn final_price_the_solve_is_confirmed_by_a_brute_force_sweep_of_the_equation() {
        let rate_sets: [Vec<&str>; 8] = [
            vec![],
            vec!["21"],
            vec!["10"],
            vec!["5"],
            vec!["100"],
            vec!["21", "10"],
            vec!["10", "5"],
            vec!["21", "10", "5"],
        ];

        for rates in rate_sets {
            for cents in 1..=20_000i64 {
                let net = Decimal::new(cents, MONEY_SCALE);
                let target = final_of(&net.to_string(), &rates);
                let answer = solve(&target.to_string(), "3", None, &rates)
                    .unwrap_or_else(|err| panic!("{target} with {rates:?} was refused: {err:?}"));

                assert_eq!(
                    final_of(&answer.net_price.to_string(), &rates),
                    target,
                    "{target} with {rates:?}: the solved net grosses to {} instead",
                    final_of(&answer.net_price.to_string(), &rates)
                );
            }
        }

        // 0.00 is the one target the sweep cannot ask for: a free price is not a
        // legal product price, so it is refused by the price rules rather than
        // solved. Asserted here so the sweep's lower bound is visibly a
        // DECISION and not an accident of the loop.
        assert_eq!(
            refusal(solve("0.00", "3", None, &[])),
            PriceRefusal::SalePriceNotPositiveForProduct
        );
    }

    /// The window is the optimisation under test, so the cross-check must not
    /// use it. Every net in a band far wider than the window is enumerated for
    /// every target in a small range, and the answer must be one of them — or,
    /// when the answer is a refusal, there must be NO net at all. A window that
    /// is too narrow, too wide, or centred wrongly fails here.
    #[test]
    fn final_price_the_solve_matches_an_exhaustive_scan_of_every_net_in_a_wide_band() {
        for rates in [
            vec!["21"],
            vec!["21", "10"],
            vec!["5"],
            vec!["10"],
            vec!["100"],
        ] {
            for cents in 1..=300i64 {
                let target = Decimal::new(cents, MONEY_SCALE);
                // 0.00 to 5.00 inclusive: five times the largest window.
                let exhaustive = every_net_with_final(&target.to_string(), &rates, 500);
                match solve(&target.to_string(), "3", None, &rates) {
                    Ok(answer) => assert!(
                        exhaustive.contains(&answer.net_price),
                        "{target} with {rates:?}: the solved net is not among the \
                         {exhaustive:?} that really produce it"
                    ),
                    Err(PriceRefusal::FinalPriceUnreachable) => assert!(
                        exhaustive.is_empty(),
                        "{target} with {rates:?}: refused, but {exhaustive:?} produce it"
                    ),
                    Err(other) => panic!("{target} with {rates:?}: unexpected {other:?}"),
                }
            }
        }
    }
}
