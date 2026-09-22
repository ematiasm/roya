# Design: add-cost-price-freshness (M1 inventory / M3 purchases)

## Derived, never stored — and this time the invariant is satisfied, not argued

Architecture invariant 3 says *"Derived state is never stored as truth"*. The sibling change
(`2026-09-21-add-markup-pricing`) stores a value derived from `cost_price` and had to argue —
correctly — why the stored price cannot contradict its own parts. This change faces the same
temptation with two new derived comparisons and takes the compliant branch instead: **nothing
here is written.**

- The drawer badge is a read-time comparison between `reference_cost(id)` and the stored
  column, computed in `product_detail_html` and passed to the template as an optional view
  struct. Nothing persists it.
- The line warning is a read-time flag computed inside the purchase detail view, derived from
  the product read the view already fetches — one product read, reused for names, stock
  tracking and the flag, so the preview cannot drift from the flows that will act on it.
- The only write in the change is `web_apply_line_cost`, and it is not a derivation write at
  all: it is an ordinary human-triggered product edit that happens to originate from the
  purchases page, routed through `InventoryService::update_product` like every other product
  write.

A cached "stale" boolean or a persisted snapshot of the disagreement would be exactly the
shape invariant 3 rejects: it would be written on a different schedule than its parts and
could claim staleness after the costs moved back together. Derived-at-read can never lie about
the current state. The cost of the choice — two extra reads at render time — is recorded under
deferred items.

## Why the body is never read

The handler takes no body extractor at all — `State`, `Require<InventoryWrite>`, the
`Principal` extension, `HeaderMap` and `Path` are its whole signature. The product and the
cost are resolved from the stored line, and the line is found **within the purchase's own
lines** (`detail.lines.iter().find(...)`). Two properties fall out, both pinned by tests:

- A client cannot inject a cost. Posting `cost_price=999&unit_cost=0.01&product_id=7` — the
  exact body an attacker would send — succeeds and stores the line's recorded 12, never the
  injected values (`web_apply_line_cost_ignores_a_client_supplied_cost`). The design decision
  came first; the test validated it the project's way, by making the lookup global in a
  throwaway copy and watching the cross-purchase test fail before tightening it.
- A line id from a different purchase is a 404 that writes nothing — both products' stored
  costs are asserted after the refusal, because a handler that answered 404 after writing
  would slip past a status-only assertion.

The cost of this choice is one status comparison in the handler (below) and the loss of the
"apply a different cost" affordance — a client that wants to correct a cost edits the line
first, then applies. Editing the record of what was actually bought is also the honest order:
the column should learn from a recorded purchase line, not from an arbitrary number.

## `InventoryWrite`, not a purchases permission

The route lives under `/web/purchases/…`, but its effect lands on a product — it is a product
write. It therefore carries `Require<InventoryWrite>` like every other product write.

`purchases.create` was the rejected gate, and the rejection is a privilege-escalation
argument, not a convention: the buyer role holds `purchases.create` to build purchase
documents. Gating a product-cost rewrite on it would hand buyers the ability to rewrite the
costs (and through a markup, the derived prices) of any product on the shop — an escalation
from "record what we bought" to "reprice the catalog". The project also reserves the
any-of grant form (`RequireAny<S>`) for read-only index screens that narrow their content to
the opener's tier; a write route is never an any-of grant.

The button is rendered unconditionally on purpose: an operator holding only
`purchases.create` sees it and, on click, gets the visible forbidden page. A silently hidden
button would deny without explanation; a rendered button that refuses makes the permission
gap visible. This mirrors the deliberate supplier-payment consequence already recorded in the
purchases spec (a `suppliers.write`-only principal sees the pay card and is refused on
submit).

## Why draft-only, enforced twice

The action only makes sense while the purchase is editable — a confirmed line's cost is frozen
history, and applying it would rewrite a product from history. The template renders the
warning only in Draft, and the handler refuses a non-draft purchase itself
(`detail.purchase.status != PurchaseStatus::Draft`). Presentation-only enforcement would be a
courtesy; the handler is the enforcement, the same triangulation every draft action takes.

Honest gap: the handler repeats the status comparison that the service's private
`ensure_draft` performs, in the service's own message shape. A shared status helper would
remove the drift risk; it was not extracted because a one-line comparison with a pinned test
did not justify touching the service's visibility. Recorded under deferred items.

## Why the badge reuses `reference_cost`

The badge must answer "what does the supplier truth say?" with the same rule every other
consumer uses: preferred supplier, else cheapest, else none. Reimplementing a `min()` in the
wiring layer would let the badge disagree with the read rule it is supposed to measure. The
gate shape follows directly:

1. `reference_cost` is `None` (no supplier rows) ⇒ no badge — the product column IS the truth
   for products the satellite never met.
2. Stored cost zero ⇒ no badge — the column is `NOT NULL DEFAULT '0'`, so zero is the schema's
   "nothing recorded yet", and comparing 7.50 against "nothing" would call a never-costed
   product stale.
3. The two differ ⇒ badge; equal ⇒ fresh.

The line warning is deliberately narrower than the badge: it fires only when the line cost is
strictly HIGHER than a real stored cost. A draft that buys cheaper is good news, not a defect
to clean up at the counter, and the standing disagreement in the other direction is already
covered by the drawer badge. Two signals with different jobs; the warning is about a drift the
operator is causing right now and can fix in one click.

## Rejected alternative: syncing the column from the satellite

The obvious "freshness" fix — have `record_cost` (or purchase confirmation) update
`products.cost_price` from the satellite — was rejected, and not on effort grounds:

- It breaks AC10. "A purchase never writes `products.cost_price`" is a stated rule of the
  purchases capability, pinned by
  `ac10_purchase_never_writes_cost_price_and_satellite_wins`, and the read rule is satellite
  first, column as fallback. Writing the column from the satellite would collapse the fallback
  distinction and change what the spec promises — a promise other rules (the fallback for
  products with no supplier row, the badge's zero gate) lean on.
- It silently reprices. A markup-derived `sale_price` would move on every purchase
  confirmation, with no human ever approving the new cost. The stored column is a
  human-approved value; the whole point of this change is to put the human where the write
  happens, which is what "Apply to product" does.

## Rejected alternative: deriving `cost_price` at read time

Making the column itself a read-time derivation from the satellite would have avoided the
drift entirely — but `cost_price` is a stored, human-approved column, the fallback for
products with no supplier row, the input the markup derivation reads at write time, and the
seed data's initial truth. Deriving it at read would introduce a dual read path (satellite
case, no-satellite case) at every consumer, and would make the column's stored value dead
weight the operator can no longer author. The column stays stored; the freshness signals are
what was added.

## Deferred items

- **The wiring guard does not cover the new route.** The smoke suite's generic form-wiring
  guard renders every seeded page and probes every `hx-post` target it finds — but its shared
  fixture seeds the purchase from the reorder suggestion at a line cost (the satellite's 7.50)
  below the product's stored cost (10), so the stale warning never renders on a guarded page
  and the new `hx-post` is never probed by the guard. The route is covered by its own route
  tests (happy path, injected body, cross-purchase 404, permission, non-draft) and the browser
  journey instead. Changing the shared fixture was deliberately declined: mutating a fixture
  that every wiring assertion leans on, to gain one more probed target, was judged too risky
  for the benefit. The gap is a limitation of breadth, not a hole in the route's coverage.
- **The handler repeats `ensure_draft`'s comparison.** The service's guard is private; the
  handler holds the same single comparison in the service's message shape rather than
  duplicating silently. A shared status helper would remove the drift risk.
- **The drawer reads supplier costs twice.** Opening the product drawer runs the supplier-cost
  listing once for the rows and once inside `reference_cost`. One indexed query on a non-hot
  path; the fix would touch the supplier service, so it was left as its own follow-up rather
  than folded into this change.
