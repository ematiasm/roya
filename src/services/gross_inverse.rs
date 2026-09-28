//! The INVERSE of the tax contract: which net grosses to a typed figure, or a
//! typed refusal saying why no net does.
//!
//! # What this module owns
//!
//! ONE definition of that question, and nothing else.
//! [`solve_net_from_gross`] is the only place in this crate where a gross
//! becomes a net, and every candidate it considers is priced through
//! [`calculate_line_taxes`] — the only definition of a gross. A caller that
//! needs the answer calls it; a caller that grew its own division would bypass
//! the staircase documented below and be wrong about the exact cent often enough
//! to be useless, which is precisely why this half was given its own name
//! instead of being copied into each place that needs it.
//!
//! What this module does NOT own is any opinion about what the figure MEANS.
//! [`solve_final_price`](crate::services::final_price::solve_final_price) is one
//! caller, and it keeps every rule that is about a sale — the price ceiling,
//! the stored markup state, the legality of the typed
//! price, and the markup itself — around this one call, in its own order. Those
//! rules are not shared, because a figure that is a cost and a figure that is a
//! sale price are not the same kind of fact and must not be refused by the same
//! sentence. Anything a caller must decide BEFORE it reaches the search belongs
//! to the caller, at the caller's own position in its own rules — which is also
//! why the divisor arrives as a parameter instead of being derived here.
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
//! # The division this introduces
//!
//! [`derive_net_sale_price`](crate::services::inventory::derive_net_sale_price)
//! states in its own comment that a percentage shift
//! is a multiplication by `0.01` because "this project never divides a
//! `Decimal`". That is true of the MARKUP derivation, and it is the reason the
//! markup half of
//! [`solve_final_price`](crate::services::final_price::solve_final_price) inverts
//! the formula and then VERIFIES the answer
//! through the real deriver instead of trusting its own arithmetic.
//! It is not true of the project as a whole: the tax contract has always
//! computed `net * rate / 100` ([`calculate_line_taxes`]). Inverting a gross is
//! a division by construction — there is no multiplication that lands on a
//! staircase from the wrong side — so this module performs the first division
//! used to DERIVE a stored price, deliberately, and says so here rather than
//! leaving the next reader to believe the comment above applies to the crate.
//!
//! # Reuse
//!
//! * [`calculate_line_taxes`] is the ONLY definition of a gross. This module
//!   never rounds a tax itself, so a solved net and a document line can never
//!   disagree by a cent about the same net.
//! * [`gross_divisor`] is the ONLY definition of the factor that turns a net
//!   into its exact, unrounded gross, and it is derived once per solve by the
//!   caller.
//!
//! Every item below is `pub` for the callers that consume the inverse — the
//! final-price solve today, the purchase boundary once it accepts a
//! tax-inclusive cost — and this unit lands the shared half FIRST, because the
//! arithmetic is the part that can be silently wrong and every later caller
//! inherits whatever it does. Until the second caller arrives there is no
//! production reader beyond the one, and this crate is a binary, where `pub` does
//! not by itself exempt an item from the dead-code pass. This is the same
//! situation, and the same remedy, as `PriceRefusal::ALL`, which carries the
//! attribute for the same reason, and as the ceiling the sale solve still owns.
#![allow(dead_code)]

use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

use crate::models::{PriceRefusal, Tax};
use crate::services::line_taxes::{calculate_line_taxes, cent, round_to_cents};

/// What a solve can fail with. There is no other failure mode: this module
/// returns a typed [`PriceRefusal`] or an answer, never a partial one and never
/// a silently different price.
///
/// It lives HERE, beside the search, and not in the sale solve that first
/// needed it. Defined there it made this module import
/// `final_price::SolveResult` while `final_price` imported this module, so the
/// extraction carried a module cycle across the boundary it existed to cut —
/// and the purchase-cost caller coming next would have inherited final-price
/// failure vocabulary through the type of its own answer. The search is the
/// shared half, so the shared half owns the shape of its own failure. Move this
/// alias back into a caller and the cycle returns with it.
pub type SolveResult<T> = Result<T, PriceRefusal>;

/// One hundred, the divisor that turns a percentage rate into a factor.
pub const PERCENT: Decimal = Decimal::ONE_HUNDRED;
/// Two, as a `Decimal`. Written out rather than imported so the arithmetic in
/// [`search_radius_cents`] reads as the fraction of a cent it is.
const TWO: Decimal = Decimal::TWO;

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

/// The net whose gross is EXACTLY `gross`.
///
/// `gross` is spelled that way rather than `final_price` because this entry
/// point is SHARED: one caller hands it a sale price and another will hand it a
/// purchase cost, and a parameter named after either of them would make the
/// other look like a misuse. The refusals below still carry final-price wording,
/// which is a separate and deliberate change rather than an oversight here.
///
/// # Why `divisor` is a parameter and not derived here
///
/// [`gross_divisor`] is called by the CALLER, at the position in the caller's
/// own rules where an uninvertible rate set is refused, and its answer is handed
/// in. That is not an accident of the signature — it is what keeps the refusal
/// ORDER observable and unchanged.
///
/// A caller's rules are an ordered list, and the order decides WHICH of several
/// true facts the caller hears. An uninvertible rate set is refused for the
/// RATES, before the typed figure is ever looked at, and a figure above the
/// arithmetic ceiling is refused for the FIGURE. Both are true at once on some
/// inputs, and the caller is the only thing that knows which one the operator
/// must fix first. Collapsing the ceiling, the divisor and the search into one
/// shared step would move the divisor check behind the caller's own legality
/// and markup-state checks, so the same product would start refusing with a
/// different sentence.
///
/// Deriving the divisor in here would therefore make the refusal order depend on
/// CALL SEQUENCE: the same fact would be reported at the search's position
/// rather than the caller's, and two callers that checked their rules in
/// different orders would answer one rate set differently. The factor is
/// derived once, at the position that owns it, and the search is handed what it
/// needs. Everything the search does with it is the same either way, because the
/// divisor is a pure function of the rate set.
///
/// # The window
///
/// The estimate `gross / divisor` is the centre; every candidate is a
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
pub fn solve_net_from_gross(
    gross: Decimal,
    taxes: &[Tax],
    divisor: Decimal,
) -> SolveResult<Decimal> {
    // The one deliberate division in a derivation: inverting a gross has no
    // multiplication, and the module docs say why this is the first one here.
    let estimate = gross
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
        if calculation.total != gross {
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
/// [`max_solvable_final_price`](crate::services::final_price::max_solvable_final_price)
/// already covers every all-NON-NEGATIVE rate set,
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

/// `1 + SUM rate_i / 100`: the factor that turns a net into its exact,
/// UNROUNDED gross. `None` when the rates gross the net away entirely, which
/// is the one rate set with no inverse at all.
///
/// Deliberately additive and never compounding, because that is what
/// [`calculate_line_taxes`] does: every rate applies to the same net.
///
/// It is `pub` because it is a SEPARATE step from the search and a caller
/// needs it at its own: the rate set has to be declared invertible at the point
/// in the caller's rules where an uninvertible set is refused, which is not
/// necessarily where the search is. See [`solve_net_from_gross`] for why that
/// separation is load-bearing.
pub fn gross_divisor(taxes: &[Tax]) -> Option<Decimal> {
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
/// The claim is a test, not a note: `gross_inverse_the_window_is_sufficient_over_
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
    use crate::models::ProductKind;
    use crate::services::final_price::{solve_final_price, FinalPriceSolve};
    use crate::services::line_taxes::MONEY_SCALE;
    use std::str::FromStr;

    fn dec(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    /// A resolved `Tax` value. The search takes resolved tax definitions, so a
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

    /// The linked taxes of a rate set, in the order they are written.
    fn taxes(rates: &[&str]) -> Vec<Tax> {
        rates
            .iter()
            .enumerate()
            .map(|(index, rate)| tax(index as i64 + 1, &format!("T{index}"), rate))
            .collect()
    }

    /// A generated rate set as the borrowed slice the solve takes, so a test can
    /// build one with a repetition count instead of writing it out.
    fn as_strs(rates: &[String]) -> Vec<&str> {
        rates.iter().map(String::as_str).collect()
    }

    /// The gross-to-net search reached the way the PRODUCT reaches it.
    ///
    /// These tests are about the search, but they are driven through the sale
    /// solve rather than through [`solve_net_from_gross`] directly, because
    /// that is where a real gross comes from: it is the one production caller
    /// the search has today, and a refusal asserted here is a refusal as an
    /// operator would meet it, after the sale solve's own rules have run. The
    /// window and the tie-break are what is under test either way — the sale
    /// half only decides WHICH refusal a rate set is given, and the arithmetic
    /// that produces the net is the search's own.
    fn solve(
        final_price: &str,
        cost_price: &str,
        stored_markup_pct: Option<&str>,
        rates: &[&str],
    ) -> SolveResult<FinalPriceSolve> {
        solve_final_price(
            dec(final_price),
            ProductKind::Product,
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
    fn gross_inverse_where_the_naive_division_misses_by_a_cent() {
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
    fn gross_inverse_where_the_division_misses_on_more_than_one_rate_set() {
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
    fn gross_inverse_a_within_a_cent_tolerance_guard_would_prove_nothing() {
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
    fn gross_inverse_ties_are_resolved_by_proximity_to_the_estimate() {
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
    fn gross_inverse_a_tie_picks_the_candidate_nearest_the_estimate_not_the_extreme() {
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

    /// THE COUNTEREXAMPLE to "a divisor above one means no ties".
    ///
    /// Rates of -5% and 10% sum to +5, so the divisor is 1.05 — comfortably
    /// above one — and yet two nets share a final price: 0.09 and 0.10 both
    /// gross to 0.10. The positive rate's contribution grows by 0.001 per cent
    /// of net and the negative one's shrinks by 0.001, and the two cancel.
    ///
    /// This test exists so the reason in `solve_net_from_gross` cannot be quietly
    /// "simplified" back to a divisor test by a future maintainer who reads it
    /// as equivalent. It is not equivalent, and this is the difference.
    #[test]
    fn gross_inverse_a_divisor_above_one_does_not_rule_out_a_tie() {
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
    fn gross_inverse_a_window_that_cannot_be_built_is_refused_not_a_panic() {
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
    fn gross_inverse_the_window_is_sufficient_over_six_hundred_of_net() {
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
}
