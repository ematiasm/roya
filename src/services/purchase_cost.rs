//! The PURCHASE boundary of the shared tax inverse: which net cost grosses to a
//! supplier's tax-inclusive figure, which cost-inclusive figure a net cost
//! grosses to, and the one mapping that turns the shared inverse's SALE-worded
//! refusals into sentences about a cost.
//!
//! # What this module owns
//!
//! Three things, and no arithmetic of its own:
//!
//! * [`solve_net_cost_from_gross`] — the conversion, delegated whole to
//!   [`solve_net_from_gross`]. It contributes the divisor (via
//!   [`gross_divisor`]) and re-words the refusals; the cent search is the
//!   shared module's, and the tax contract inside it is
//!   [`calculate_line_taxes`], the only definition of a gross in this crate.
//! * [`gross_cost_from_net`] — the OTHER direction, which is not a conversion
//!   at all: it is the tax contract applied to a unit cost, and it carries that
//!   contract's own refusal through untouched.
//! * [`cost_ask`] — THE DIRECTION RULE: of a typed pair, which side is the
//!   input. It is pure, does no arithmetic, and exists so the rule cannot be
//!   restated per surface.
//!
//! # Why the refusals are re-worded here and not in the route
//!
//! `solve_net_from_gross` is SHARED — the sale solve is its first caller and
//! this is its second — so its refusals say "final price", which is the figure
//! the SALE reads. A supplier's tax-inclusive cost is a different kind of fact
//! and must not be refused with a sentence that sends the operator to correct a
//! final price nobody typed. The four cost variants already exist for exactly
//! this boundary; nothing in this crate produced one until now.
//!
//! The mapping is here, in the service, rather than in a route's `match`, for
//! the reason every other shared rule in this crate is: the REST surface
//! (`purchases_api.rs`) accepts a line cost too, and a route-local mapping
//! would leave it answering a sale sentence. One mapping, every caller.
//!
//! # The direction rule, and what a browser without JavaScript gets
//!
//! [`CostAsk::Net`] is the DEFAULT and it is deliberate. The net is the stored
//! truth (the feature's decision 1), and the net field is the one the save path
//! has always read. A form that states no basis therefore posts exactly what it
//! posted before this pair existed — including create's supplier-satellite
//! fallback, which needs [`CostAsk::Unstated`] rather than a guess.
//!
//! The gross becomes the input only when the form SAYS so, through the basis the
//! page carries, or when the net is absent and the gross is the only figure on
//! offer. Both cases are refusable by the same solve, so a staircase gap
//! refuses identically in the preview and in the write: the operator is never
//! shown a net the write would not accept.
//!
//! # The read skew, stated rather than frozen away
//!
//! The caller resolves the tax set at ROUTE time, through the same
//! `list_active_for_product` read the ladder uses, and the write then re-resolves
//! it INSIDE its own transaction (`purchase_repo::active_taxes_for_product`).
//! A tax linked or deactivated between those two moments is a real, narrow
//! hazard, and it is accepted on purpose: the snapshot logic resolves
//! in-transaction so a document can never be written against a rate set that
//! moved under it, and freezing a `Vec<Tax>` into the write to make the
//! preview and the write agree would trade a real correctness property for a
//! cosmetic one. The post-write re-render is the truth the operator actually
//! sees, and it is read back from the database.
use rust_decimal::Decimal;

use crate::models::{PriceRefusal, Tax};
use crate::services::gross_inverse::{
    gross_divisor, solve_net_from_gross, SolveResult,
};
use crate::services::line_taxes::calculate_line_taxes;

/// Which side of the pair the operator typed into last.
///
/// It travels as a form field the page sets, and it is [`None`] whenever the
/// page did not say — a browser with no JavaScript, a REST client, the
/// collection adapters. The rule that reads it is in [`cost_ask`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostBasis {
    Net,
    Gross,
}

impl CostBasis {
    /// The basis a form field states, or `None` for anything else.
    ///
    /// Deliberately total and never an error: an absent, blank or unrecognised
    /// marker all mean the same thing — the page did not say — and they all
    /// resolve to the net, which is the stored truth. A value this application
    /// did not write must not be able to select a basis.
    pub fn parse(raw: &str) -> Option<CostBasis> {
        match raw.trim() {
            "net" => Some(CostBasis::Net),
            "gross" => Some(CostBasis::Gross),
            _ => None,
        }
    }
}

/// What a typed pair asks for, before any arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostAsk {
    /// The net is the input and it is on offer. No tax set is read and nothing
    /// is solved, so a valid net is never refused by anything about the gross.
    Net(Decimal),
    /// The gross is the input; the net is what has to be found.
    Gross(Decimal),
    /// Neither side carries a figure. The caller keeps its own answer: create
    /// falls back to the supplier satellite and then the product column, and
    /// the inline edit refuses, because there the net is required.
    Unstated,
}

/// THE DIRECTION RULE. Of a typed pair, which side is the input.
///
/// One definition, and it is pure: no read, no arithmetic, no `AppError`. The
/// two write paths (create and the inline edit) and the entry row's preview
/// all call it, so the rule cannot drift between them — the failure this
/// prevents is a create that accepts a gross and an edit that quietly ignores
/// it, which is invisible until two operators get two different documents out
/// of the same shelf.
///
/// The precedence, and the reason for each step:
///
/// 1. A stated basis that names a field which actually carries a figure wins.
///    That is "whichever the operator typed into becomes the input", and it is
///    the only case in which the gross is read while a net is also present.
/// 2. Otherwise a present net wins. This is the no-JavaScript path and it is
///    today's path: the net field is the one the save has always read, so a
///    stale basis marker can never make a typed net invalid, and a gross that
///    happens to be sitting in the other field can never refuse it.
/// 3. Otherwise a present gross wins, because it is the only figure on offer.
/// 4. Otherwise nothing was typed.
pub fn cost_ask(
    basis: Option<CostBasis>,
    net: Option<Decimal>,
    gross: Option<Decimal>,
) -> CostAsk {
    let stated = |want: CostBasis, value: &Option<Decimal>| -> Option<Decimal> {
        if basis == Some(want) {
            *value
        } else {
            None
        }
    };
    if let Some(value) = stated(CostBasis::Gross, &gross) {
        return CostAsk::Gross(value);
    }
    if let Some(value) = stated(CostBasis::Net, &net).or(net) {
        return CostAsk::Net(value);
    }
    if let Some(value) = gross {
        return CostAsk::Gross(value);
    }
    CostAsk::Unstated
}

/// The net cost whose gross is EXACTLY `gross`.
///
/// # What this adds to the shared solve
///
/// Two things, both of them boundary decisions rather than arithmetic:
///
/// * the divisor, derived here because this boundary has no other rule to put
///   it in — an uninvertible rate set is refused for the RATES, before the
///   typed figure is looked at, which is where the sale solve puts it too; and
/// * the refusal vocabulary, through [`cost_refusal`].
///
/// The cent search is [`solve_net_from_gross`]'s, unchanged and un-wrapped:
/// for every reachable figure the inverse is exact, and a staircase gap is
/// refused rather than approximated. This function does not round, does not
/// divide and does not approximate.
pub fn solve_net_cost_from_gross(gross: Decimal, taxes: &[Tax]) -> SolveResult<Decimal> {
    // Derived HERE and refused HERE, for the reason the sale solve documents
    // on its own rule 2: the rates are the thing that has to change before
    // anything about this figure can be judged.
    let divisor = gross_divisor(taxes).ok_or(PriceRefusal::CostNotInvertible)?;
    solve_net_from_gross(gross, taxes, divisor).map_err(cost_refusal)
}

/// The cost-inclusive figure a net cost grosses to.
///
/// The OTHER direction, and it is a derivation rather than a conversion, so
/// there is nothing to invert and nothing to search: this is
/// [`calculate_line_taxes`] applied to a UNIT cost, the same call the price
/// ladder's `cost_total` makes, and the same call a line write will make when
/// it stores the figure.
///
/// The refusal is the tax contract's OWN and is carried through untouched, for
/// a reason worth stating because the mapping above deliberately re-words
/// three others: the cost ladder's `cost_refusal` is the same fact about the
/// same number and shows this sentence, and one figure that is reported two
/// ways is one figure nobody can trust. The four `Cost*` variants exist for the
/// CONVERSION — for a search that can fail to find anything — and this direction
/// has no search to fail.
pub fn gross_cost_from_net(net: Decimal, taxes: &[Tax]) -> SolveResult<Decimal> {
    calculate_line_taxes(net, taxes).map(|calculation| calculation.total)
}

/// ONE mapping from the shared inverse's sale-worded refusals to the cost
/// boundary's own. Exhaustive on the three the inverse can produce, with a
/// documented fallback for anything else.
///
/// | the inverse says | the cost boundary says |
/// |---|---|
/// | `FinalPriceUnreachable` (a staircase gap) | `CostUnreachable` |
/// | `NetPriceTooLarge` (the window leaves the range) | `CostNetTooLarge` |
/// | `TaxRateTooLargeToPrice` (one linked rate is too large) | `TaxRateTooLargeToCost` |
///
/// The fallback arm is not a shrug. The closure it stands on is a TEST, not a
/// comment — `the_refusal_mapping_never_widens_one_of_the_shared_inverses_own`
/// sweeps rate sets and targets and fails if the shared inverse ever refuses
/// something outside the three — because the compiler cannot see a function's
/// output set. `CostUnreachable` is the honest landing for an unrecognised
/// refusal: it is the sentence that says "no net cost grosses to this cost",
/// which is the direction the operator was working in, and it is a sentence the
/// operator can act on rather than one about a final price they never typed.
pub fn cost_refusal(refusal: PriceRefusal) -> PriceRefusal {
    match refusal {
        PriceRefusal::FinalPriceUnreachable => PriceRefusal::CostUnreachable,
        PriceRefusal::NetPriceTooLarge => PriceRefusal::CostNetTooLarge,
        PriceRefusal::TaxRateTooLargeToPrice => PriceRefusal::TaxRateTooLargeToCost,
        other => {
            // Pinned by the sweep named above. A `debug_assert` would make a
            // production build silently mislabel a refusal, so the arm returns
            // a real cost refusal instead.
            debug_assert!(
                false,
                "the shared inverse refused {other}, outside the three the cost mapping names"
            );
            PriceRefusal::CostUnreachable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::line_taxes::round_to_cents;
    use std::str::FromStr;

    fn dec(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn tax(rate: &str) -> Tax {
        let midnight = chrono::NaiveDate::from_ymd_opt(2024, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        Tax {
            id: 1,
            code: "IVA".into(),
            name: "IVA".into(),
            rate: dec(rate),
            is_active: true,
            created_by: 0,
            updated_by: None,
            created_at: midnight,
            updated_at: midnight,
        }
    }

    fn taxes(rates: &[&str]) -> Vec<Tax> {
        rates.iter().map(|rate| tax(rate)).collect()
    }

    #[test]
    fn a_gross_answers_the_net_whose_gross_is_exactly_that_figure() {
        let taxes = taxes(&["21"]);
        assert_eq!(
            solve_net_cost_from_gross(dec("6.05"), &taxes).unwrap(),
            dec("5.00")
        );
    }

    #[test]
    fn the_solved_net_reprices_to_the_typed_gross_and_not_to_a_neighbour() {
        for (gross, rates) in [
            ("0.01", ["21"].as_slice()),
            ("0.02", ["21"].as_slice()),
            ("0.99", ["21"].as_slice()),
            ("1.21", ["21"].as_slice()),
            ("3.63", ["21"].as_slice()),
            ("6.05", ["21"].as_slice()),
            ("121.00", ["21"].as_slice()),
            ("242.00", ["21"].as_slice()),
            ("100.00", ["10", "5"].as_slice()),
            ("100.00", ["21", "5"].as_slice()),
            ("0.23", ["21", "10"].as_slice()),
            ("5.00", [].as_slice()),
        ] {
            let linked = taxes(rates);
            let net = solve_net_cost_from_gross(dec(gross), &linked).unwrap_or_else(|refusal| {
                panic!("{gross} with {rates:?} must be reachable: {refusal}")
            });
            let repriced = calculate_line_taxes(net, &linked).unwrap().total;
            assert_eq!(
                repriced,
                dec(gross),
                "the net {net} reprices to {repriced}, not the typed {gross} with {rates:?}"
            );
        }
    }

    /// THE ROUND TRIP, pinned to the CENT, on the rate sets where a plain
    /// division is known to land a cent away.
    ///
    /// `the_solved_net_reprices_to_the_typed_gross_and_not_to_a_neighbour` above
    /// could be satisfied by an answer that is merely self-consistent, and a
    /// divide-and-round inverse is self-consistent on most figures: with 21%
    /// alone, `gross / 1.21` usually rounds to the very net the search finds.
    /// These three are the fixtures the sale suite already found by
    /// exhaustively comparing the division against the equation, and each one
    /// separates the two answers by exactly a cent. They are here so the
    /// exactness of this boundary is a pinned property rather than an
    /// assumption: replace the search with `round2(gross / divisor)` and this
    /// test names the cent it got wrong.
    #[test]
    fn the_solved_net_is_not_the_cent_a_plain_division_lands_on() {
        for (gross, rates, exact_net, division_net) in [
            ("100.00", ["10", "5"].as_slice(), "86.95", "86.96"),
            ("100.00", ["21", "5"].as_slice(), "79.36", "79.37"),
            ("0.23", ["21", "10"].as_slice(), "0.17", "0.18"),
        ] {
            let linked = taxes(rates);
            assert_eq!(
                solve_net_cost_from_gross(dec(gross), &linked).unwrap(),
                dec(exact_net),
                "{gross} with {rates:?} must solve to {exact_net}"
            );
            // The fixture, asserted rather than assumed: the division really
            // does land on the other cent here, which is what makes the search
            // load-bearing at this boundary.
            let divisor = gross_divisor(&linked).unwrap();
            assert_eq!(
                round_to_cents(dec(gross).checked_div(divisor).unwrap()),
                dec(division_net),
                "{gross} with {rates:?}: the division's answer, which is not the exact one"
            );
        }
    }

    #[test]
    fn a_gross_that_is_the_gross_of_no_net_is_refused_rather_than_approximated() {
        // 21% alone: 0.01 grosses to 0.01 and 0.02 grosses to 0.02, but 0.03
        // grosses from no net at all — the per-contribution rounding leaves a
        // flat step there.
        let taxes = taxes(&["21"]);
        assert_eq!(
            solve_net_cost_from_gross(dec("0.03"), &taxes),
            Err(PriceRefusal::CostUnreachable)
        );
        // And the refusal is not a general failure of the rate set: both
        // neighbours are reachable, so nothing here is a rounding artefact.
        assert_eq!(
            solve_net_cost_from_gross(dec("0.02"), &taxes).unwrap(),
            dec("0.02")
        );
        assert_eq!(
            solve_net_cost_from_gross(dec("0.04"), &taxes).unwrap(),
            dec("0.03")
        );
    }

    #[test]
    fn every_cost_refusal_is_reachable_from_a_real_typed_gross() {
        // 100 taxes at `-0.99999999999` gross the figure down by exactly 1e-11,
        // so this gross divides back to `Decimal::MAX`: the estimate fits and
        // the window's upper endpoint does not.
        let crushing: Vec<Tax> = std::iter::repeat(tax("-0.99999999999")).take(100).collect();
        assert_eq!(
            solve_net_cost_from_gross(dec("792281625142643375.93543950335"), &crushing),
            Err(PriceRefusal::CostNetTooLarge)
        );
        // Rates summing to -100 have no divisor at all.
        let inverted = taxes(&["-100"]);
        assert_eq!(
            solve_net_cost_from_gross(dec("10.00"), &inverted),
            Err(PriceRefusal::CostNotInvertible)
        );
        // A mixed-sign set whose cancellation hides one enormous rate.
        let mixed = taxes(&["100000000000000000000", "-100000000000000000000"]);
        assert_eq!(
            solve_net_cost_from_gross(dec("10000000000"), &mixed),
            Err(PriceRefusal::TaxRateTooLargeToCost)
        );
    }

    #[test]
    fn the_refusal_mapping_never_widens_one_of_the_shared_inverses_own() {
        // The closure the mapping's fallback arm rests on, as a claim rather
        // than a comment: over a sweep of rate sets and targets, the shared
        // inverse only ever refuses these three.
        let rate_sets: [Vec<&str>; 9] = [
            vec![],
            vec!["21"],
            vec!["10"],
            vec!["21", "10"],
            vec!["0.5"],
            vec!["100"],
            vec!["-50"],
            vec!["-100"],
            vec!["100000000000000000000", "-100000000000000000000"],
        ];
        let targets = [
            "0.00", "0.01", "0.02", "0.03", "1.00", "6.05", "121.00", "10000000000.00",
        ];
        for rates in &rate_sets {
            let set = taxes(rates);
            let Some(divisor) = gross_divisor(&set) else {
                continue;
            };
            for target in targets {
                if let Err(refusal) = solve_net_from_gross(dec(target), &set, divisor) {
                    assert!(
                        matches!(
                            refusal,
                            PriceRefusal::FinalPriceUnreachable
                                | PriceRefusal::NetPriceTooLarge
                                | PriceRefusal::TaxRateTooLargeToPrice
                        ),
                        "the shared inverse refused {refusal} for {target} with {rates:?}: a \
                         refusal outside that set would fall through the cost mapping's fallback \
                         arm and be reported as a staircase gap"
                    );
                }
            }
        }
    }

    #[test]
    fn a_net_answers_its_gross_through_the_one_line_tax_contract() {
        let taxes = taxes(&["21"]);
        assert_eq!(
            gross_cost_from_net(dec("5.00"), &taxes).unwrap(),
            dec("6.05")
        );
        // A rate set with no linked tax grosses to the net itself.
        assert_eq!(
            gross_cost_from_net(dec("5.00"), &[]).unwrap(),
            dec("5.00")
        );
    }

    #[test]
    fn a_net_too_large_to_price_keeps_the_line_tax_contracts_own_refusal() {
        // Deliberately NOT one of the Cost* variants: the ladder's `cost_refusal`
        // is the same fact and carries the same sentence, and the Cost* variants
        // exist for the CONVERSION, which this direction is not.
        let taxes = taxes(&["21"]);
        assert_eq!(
            gross_cost_from_net(Decimal::MAX, &taxes),
            Err(PriceRefusal::TaxArithmeticTooLarge)
        );
    }

    #[test]
    fn the_net_is_the_input_until_the_form_says_the_gross_is() {
        // No stated basis: the net wins, so a post from a browser with no
        // JavaScript is exactly today's post.
        assert_eq!(
            cost_ask(None, Some(dec("5.00")), Some(dec("6.05"))),
            CostAsk::Net(dec("5.00"))
        );
        assert_eq!(
            cost_ask(Some(CostBasis::Net), Some(dec("5.00")), Some(dec("6.05"))),
            CostAsk::Net(dec("5.00"))
        );
        // The operator typed the gross last, so the gross is the input.
        assert_eq!(
            cost_ask(Some(CostBasis::Gross), Some(dec("5.00")), Some(dec("6.05"))),
            CostAsk::Gross(dec("6.05"))
        );
    }

    #[test]
    fn an_absent_cost_is_never_reported_as_a_basis() {
        assert_eq!(
            cost_ask(None, None, None),
            CostAsk::Unstated,
            "create's satellite fallback needs the Unstated state"
        );
        assert_eq!(
            cost_ask(Some(CostBasis::Gross), Some(dec("5.00")), None),
            CostAsk::Net(dec("5.00")),
            "a basis naming a field the operator emptied falls back to the net, so a stale \
             marker can never refuse a net the operator did type"
        );
        assert_eq!(
            cost_ask(None, None, Some(dec("6.05"))),
            CostAsk::Gross(dec("6.05")),
            "a gross with no basis stated is still the only figure on offer"
        );
        assert_eq!(
            cost_ask(Some(CostBasis::Gross), None, None),
            CostAsk::Unstated
        );
    }

    #[test]
    fn a_basis_is_read_from_the_form_and_never_guessed() {
        assert_eq!(CostBasis::parse(""), None);
        assert_eq!(CostBasis::parse("   "), None);
        assert_eq!(CostBasis::parse("net"), Some(CostBasis::Net));
        assert_eq!(CostBasis::parse(" gross "), Some(CostBasis::Gross));
        assert_eq!(
            CostBasis::parse("total"),
            None,
            "an unknown basis is no basis"
        );
    }
}
