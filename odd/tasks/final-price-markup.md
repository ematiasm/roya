# Set a product's final price and let the markup follow

## Objective

Let an operator type a product's final, tax-inclusive price and have the system solve for the net price and rewrite the markup, so the ladder keeps showing a coherent cost → markup → net → tax → final chain.

## Problem

The pricing chain is one-directional today: cost and markup derive the net, taxes derive the final price. There is no way to express the opposite intent — "this product should sell at 100 including tax" — so the operator has to mentally invert a calculation that includes per-tax rounding.

That inversion is not a formula. The final price is `round(2, net + Σ round(2, net × rateᵢ/100))`, so each tax is rounded on its own. Dividing the target by `1 + Σrates/100` lands within a cent or two of the right answer often enough to be useless: the operator types 100 and the ladder shows 100.01, which is a lie about what they asked for.

## Decisions

- **One-shot conversion, not a pricing mode.** The stored net stays canonical, no schema change and no migration, and document lines are unaffected — they snapshot the net exactly as they do today. Changing a tax later still moves the final price, which is the already-documented consequence of the net being the truth.
- **The solve is a bounded search, not a division.** An initial estimate places the window; a small cent-range search then finds a net whose final price is **exactly** the typed value. A division alone cannot guarantee that with per-contribution rounding.
- **Tie-break by proximity to the estimate.** More than one net can produce the same final price. The one closest to the unrounded estimate is chosen, because it is the one that best represents the operator's intent. The rule is stated where the choice is made, with the evidence, not as "whichever the loop finds first".
- **The markup round trip is verified, not assumed.** After solving the markup the real deriver is run again and compared. If it does not reproduce the solved net, precision is increased and the attempt retried, up to a bound. If it never closes, the request is refused with a typed message rather than storing a price that is not the one asked for.
- **Zero cost derives no markup, and says so.** A manual product with no cost can have its final price set — the net is solved and no markup is invented — and the ladder states the markup is not derivable without a cost. A product that already carries a markup and has no positive cost is refused, because the stored markup and the stored net would contradict each other and the next save would silently undo the change.
- **A manual product becomes markup-managed.** Setting a final price writes `markup_pct`, which is what "the markup updates automatically" means; a manual product that gets a final price is no longer manual, and the ladder says so.
- **One control, its own route.** The final price is a separate small form beside the ladder, not another field in the main save form, because it means something different: the save form expresses cost and markup, this expresses the price the customer pays. It follows the same pattern as the existing cost-recording control.
- **The ladder shows the truth after the conversion.** If the markup was rounded on the way, the ladder says the derived price is the stored net's consequence, not a second free number.
- **Unreachable targets are refused, never approximated.** A final price so small that no net produces it is a typed refusal, not a silently different price.
- Technical artifacts, code, comments, tests, and UI copy are English.

## Scope

### In scope

- A pure, total solve: final price → net price, and net price → markup, with a verified round trip.
- Typed refusals for every state the solve cannot honour, rendered through the existing single refusal renderer.
- One route and one ladder control, gated `InventoryWrite`, read-only in effect: the preview never persists.
- The conversion write, which rewrites net and markup in one transaction and re-renders the drawer.
- Focused tests plus a Playwright test that types a final price and reads the resulting ladder.

### Out of scope

- Any schema change or migration.
- Any change to sale or purchase document math, the tax snapshot contract, or the net-is-canonical decision.
- A JSON API twin for the conversion route; recorded as a deliberate omission, since the request is about the product screen and an unwritten API surface is better than a written one nobody asked for.
- Locking the final price against later tax changes.

## Authorized scope

Repository-local service, route, template, localization, test, and feature-document changes. No push, PR creation, or merge without explicit user request. No database reset, no migration, and no change to the development database.

## Route declaration

Two bounded delegated-direct ODD work units. U1 is the arithmetic and is deliberately separated from the wiring, because it is the part that can be silently wrong: a solve that returns 100.01 for a typed 100 looks exactly like a correct one in a screenshot. One writer per unit. The parent owns task closure, commits, verification, and delivery reporting. No SDD phase is created.

## TDD mode

- Effective mode: strict TDD inherited from project configuration (`openspec/config.yaml`, `strict_tdd: true`).
- Test runner: `cargo test`, plus the Playwright harness for the control.
- The RED must be behavioural and must include the cases where a naive division is wrong, or the guard proves nothing.

## Work units

- [ ] U1 — The pure solve, with typed refusals and a verified round trip.
  - Tests first, observed RED, including the cases where division alone misses.
  - Solve the net from a target final price across zero, one and several additive taxes.
  - Solve the markup from that net, then verify the round trip through the real deriver and refine precision within a bound.
  - Cover unreachable targets, zero cost with and without an existing markup, and the tie-break rule.
  - Evidence: RED/GREEN, exact commands, the brute-force cross-check, commit identity.

- [ ] U2 — The ladder control and the conversion write.
  - One route, gated `InventoryWrite`, preview that never persists and a write that rewrites net and markup together.
  - The control in the ladder, the truthful post-conversion display, and the localized copy for every refusal.
  - A Playwright test that types a final price and reads the resulting ladder figures.
  - Evidence: RED/GREEN, exact commands, commit identity.

## Acceptance criteria

1. Typing a final price yields a stored net whose derived final price is exactly the typed value, for zero, one and several taxes.
2. The markup is rewritten and re-deriving from it reproduces the stored net.
3. A case where a naive division misses by a cent is covered by a test, and the solve lands on the typed value.
4. More than one valid net is resolved by the documented tie-break, not by loop order.
5. A target no net can produce is refused with a typed, localized message and nothing is written.
6. A manual product with a cost becomes markup-managed, and the ladder says so.
7. A product with no cost takes a final price without inventing a markup, and the ladder states the markup is not derivable.
8. A product that already has a markup and no positive cost is refused, with nothing written.
9. The preview persists nothing; the conversion write is atomic and audited like every other product mutation.
10. Documents are unaffected: a new line still snapshots the net, and existing lines never move.
11. Focused tests, the full Rust suite, and the applicable browser checks pass, with every skip recorded.

## Applicable checks

- `cargo test price_ladder`
- `cargo test product_price_ladder`
- `cargo test product`
- `cargo test final_price`
- `cargo test`
- `cargo check --all-targets`
- `cargo fmt --check`
- `bash scripts/e2e.sh tests/test_products.py`
- `bash scripts/e2e.sh tests/test_visual_baseline.py`
- `git diff --check`

## Progress and evidence

- Baseline: branch `main` at `23f548d`, working tree clean apart from the unrelated untracked `odd/tasks/pos-counter-sales.md`, which must never be staged by this task.
- **Corrected 2026-09-26, and the original claim here was false.** This document first said the project has "no `Decimal` division anywhere", taken from the comment at `src/services/inventory.rs:67`. That comment is itself an overstatement: `src/services/line_taxes.rs:149` computes `amount = net * rate / 100`, so the tax path has always divided a `Decimal`. The accurate statement is narrower: **the markup path avoids division by scaling with a multiplication by `0.01`, and there is no `checked_div` anywhere in the repository** — division by a *variable* does not yet appear. That comment in `inventory.rs` is a misleading claim this work's U1 did not fix, and it is recorded as a follow-up rather than silently rewritten.
- Verified constraint: the final price is `round(2, net + Σ round(2, net × rateᵢ/100))` with per-contribution rounding, so inverting it is a fixed point rather than a formula.
- Verified constraint: a markup without a positive cost is already refused, and a derived price that pins to zero is already refused.
- Feature document: `odd/tasks/final-price-markup.md`.
- Engram mirror topic: `odd/final-price-markup/tasks`.
- Next step: U1 strict-TDD solve.
