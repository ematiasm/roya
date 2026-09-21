# Design: add-markup-pricing (M1 inventory)

## The invariant-3 tension, confronted

Architecture invariant 3 says *"Derived state is never stored as truth… Where a stored value could
contradict its own parts, it must not be stored"*, and cites a receipt's stored total that was
removed for exactly that reason. This change stores a price derived from `cost_price` and
`markup_pct`, so it sits in tension with the invariant and the tension has to be argued, not
ignored.

The honest argument:

- `sale_price` is not a cache of a derivation — it is the authoritative price the operator
  approved. The derivation is a convenience for *setting* it, not a live source of truth for
  *reading* it. Because the recomputation happens at write time, at rest a product with a markup
  always holds exactly the derived price: the stored value cannot contradict its own parts, which
  is the property invariant 3 protects. The receipt total failed that property because it was
  written on a different schedule than the parts it summarised; a price written by the same
  validated write as its parts does not.
- The alternative — deriving at read time — would force a dual read path (stored for the manual
  case, computed for the markup case) everywhere a price is read, and would let the charged price
  move with no write at all: change the cost, and every marked-up product silently reprices. A
  price that moves with no write is worse than a stored price that is consistent by construction.
- The accepted cost: this consistency is a service-level invariant, not a database CHECK, because
  SQLite cannot do decimal arithmetic on TEXT columns. A write that bypassed
  `InventoryService` could break it. The limitation is stated here plainly; in practice every
  product write goes through the service, and sale lines snapshot their price when a line is
  built, so no historical document depends on the invariant holding retroactively.

## Markup over cost, not margin over the sale price

The rejected alternative was commercial margin: `sale_price = cost / (1 - m/100)`. It is
non-terminating (`100 / 0.7 = 142.857…`), needs a division-by-zero guard at `m = 100`, and would
have introduced the project's first `Decimal` division — an operation the codebase has never
performed and still does not perform. The markup formula is multiplication only, always
terminating, always exact before the cents pin.

The tradeoff accepted in exchange is lexical: the number the operator types is a markup *over
cost*, not the commercial *margin over the sale price* many retailers think in (a 25% margin is a
33.3% markup). The field is labelled "Markup %" so the word matches the arithmetic, and the UI
hint says the price is derived from the cost.

## Multiplication, never division

The percentage's scale shift is applied as `m * Decimal::new(1, 2)`, never `m / 100`. The project
has never divided a `Decimal` anywhere and still does not; introducing one for a constant scale
shift would be a language change, not a necessity.

## The first rounding, and why half-up

Until now every money operation in the project was exact: multiplying an exact quantity by an
exact price never produced a third decimal, so no `round_dp` existed anywhere. A percentage is the
first operation that can (`80.00 * 1.3333 = 106.6640`), so the derived value is pinned to cents
here — and only here. Manual prices keep the exact value the caller sent.

The strategy is `MidpointAwayFromZero` (half-up away from zero, the retail convention). It is
pinned by a midpoint test (`10.005 → 10.01`), because the other rounding tests round a
non-midpoint *down* and would pass under any strategy: a test that cannot discriminate the chosen
strategy proves nothing about it. The one earlier coverage gap — the original rounding test used a
non-midpoint — was found in verification and closed with the midpoint test.

## NULL versus 0

`markup_pct` is nullable TEXT and `NULL` is a real value: "no markup, the price is manual". It is
deliberately not `0`, because a 0% markup would pin the price to the cost — a very different
statement. NULL is also the landing state of "clear", which keeps the last stored price instead of
reverting anything.

## The strict parser

The column is read with `parse_decimal_opt_strict`, a strict sibling of `parse_decimal_opt`. The
loose parser maps a malformed stored value to `Decimal::ZERO`; for markup that ZERO is a
*meaningful* value — it would silently mean 0% and pin the price to the cost. The strict parser
degrades a malformed `markup_pct` to `None` ("no markup, manual price") instead, leaving the
stored price alone.

## The readonly field is courtesy

The two web forms do not behave the same, and the courtesy is arranged differently in each. In the
drawer (`templates/partials/product_detail.html`), the server renders the price input `readonly`
when the product has a stored markup, shows the stored price, and carries the hint that it is
recalculated from the cost on save. In the create modal (`templates/products.html`) there is no
stored product — no derived value to show and no hint — so the price input's `readonly` state is
set entirely by a small client-side script that locks the field the moment a markup is typed.

The `readonly` attribute is courtesy only — the handler is the enforcement: the server ignores the
submitted price whenever a markup is set. The field cannot lie meaningfully, which is why it is
allowed to look like the source of truth.

A concrete example of why the state is courtesy rather than enforcement: the modal's script
initially survived the form's `reset()`, because a reset restores values and fires no `input`
event, so after creating a product with a markup the price field stayed readonly with the markup
field already empty again. The next manual create could not be typed into and submitted an empty
price, which the handler rejected with a 400 that htmx ignores — a silent dead end. Commit
`e76882a` fixed it by making the sync listen for the form's reset and defer itself one tick (the
reset event fires before the browser restores the values); the regression test was validated by
reintroducing the defect and watching it fail.

The number only moves on save, deliberately: the price shown while editing is the price that is
currently stored — what the last save decided — not a live prediction of what this edit will
produce. Recomputing client-side would let the display drift from the server's arithmetic (same
formula in two languages) and would imply a write that has not happened.

## The deliberate divergence

A markup deriving a `0.00` price is refused for a `Product` but stored for a `Service`, because
the pre-existing rule allows a free service (`sale_price >= 0` while `Product` demands `> 0`).
The derivation changes the effective price but not the rule that judges it; forcing the product
rule onto services would have been a second, unstated behaviour change. Recorded as a decision,
not an accident.

## Deferred items

From the proposal's out of scope:

- Keeping `cost_price` fresh from the preferred supplier's cost — planned in
  `odd/tasks/cost-price-freshness.md`. The markup is only as honest as the cost underneath it, and
  `validate_product` re-derives on a cost-only patch precisely so that feature can reuse the
  formula in one place.
- Normalising the money scale at the remaining supplier-cost display sites (the drawer's
  per-supplier cost rows and the purchase line unit cost): display polish, no behaviour.
