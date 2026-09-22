# Spec: add-cost-price-freshness (M1 inventory / M3 purchases)

Delivered on `feat/cost-price-freshness`; these rules are the present-tense truth in
`openspec/specs/inventory/spec.md` (the products badge) and `openspec/specs/purchases/spec.md`
(the apply-cost route and its authorization consequence). This delta extends two existing
capabilities; no existing rule was changed or removed — in particular the purchases rule "a
purchase never writes `products.cost_price`" is untouched.

## Rules

- **The product drawer carries a permanent stale-cost badge, derived and never stored.** It
  compares `SupplierService::reference_cost(product_id)` — the preferred supplier's current
  cost, else the cheapest, else none — against the stored `cost_price`, and shows both values
  (`reference $X • stored $Y`).
- **Three badge gates.** The badge appears only when (1) a reference cost exists — with no
  supplier rows the product column IS the truth and there is nothing to compare against; (2)
  the stored cost is not zero — the column is `NOT NULL DEFAULT '0'`, so zero means "no cost
  recorded yet", not a cost; (3) the two genuinely differ. Equal costs are fresh, not stale.
- **A draft purchase line whose cost is strictly higher than the product's non-zero stored
  cost warns.** The warning shows both numbers (line cost and stored cost) and renders only in
  Draft. Strict inequality: equal or lower is not what it warns about — the drawer badge
  covers any other disagreement. The flag is derived from the product read the purchase
  detail view already fetches; nothing is stored.
- **"Apply to product" is the warning's action.** `POST
  /web/purchases/{purchase_id}/lines/{line_id}/apply-cost` writes the line's cost into the
  product via `InventoryService::update_product` with a patch carrying only `cost_price`, so a
  markup-derived `sale_price` is recomputed by the existing derivation.
- **The request body is never read.** The handler takes no body extractor: the product and the
  cost are resolved from the stored line, found within the purchase's own lines. A client
  cannot inject a cost, and a line id from a different purchase is a 404.
- **The action is gated `inventory.write` and draft-only.** The handler refuses a non-draft
  purchase itself, in the service's own message shape. The button renders for every draft
  viewer by design; an operator holding only `purchases.create` sees a visible forbidden page
  rather than a dead end.
- **The purchase flow still never writes the column.** The action is a separate,
  human-triggered product write that reaches the product through the inventory service. The
  purchases rule and its pinning test (`ac10_purchase_never_writes_cost_price_and_satellite_wins`)
  are untouched.

## Interface

- Web, inventory: the product drawer's supplier-costs card carries the stale-cost badge when
  the three gates hold.
- Web, purchases: `POST /web/purchases/{purchase_id}/lines/{line_id}/apply-cost` (HTMX
  `hx-post` from the draft line's warning sub-row; full-page redirect fallback). No REST
  routes; no stored state.

## Acceptance criteria

- [ ] AC1: the drawer shows the stale-cost badge with both values when the reference cost
      differs from the stored cost.
- [ ] AC2: equal costs hide the badge even though a supplier row exists — fresh, not stale.
- [ ] AC3: a product with no supplier rows never shows the badge: with no satellite rows the
      product column IS the truth.
- [ ] AC4: a stored cost of zero never shows the badge, even when a reference cost exists:
      zero is "no cost recorded yet", not a comparable cost.
- [ ] AC5: the badge uses the preferred supplier's cost when one is marked — never a cheapest
      fallback of its own. It reuses `reference_cost`'s rule.
- [ ] AC6: a draft line whose unit cost is strictly higher than the product's non-zero stored
      cost carries the stale-cost warning with both numbers; equal, lower and zero-stored
      lines do not warn.
- [ ] AC7: applying the cost writes the line's `unit_cost` into the product's `cost_price`
      and recomputes a markup-derived `sale_price` from the new cost, never leaving the
      derived price stale behind the new cost.
- [ ] AC8: the handler never reads a request body: posting an empty body succeeds, and a
      posted `cost_price` or `unit_cost` field is ignored — the cost comes from the stored
      line.
- [ ] AC9: a line id from a different purchase returns 404 and writes nothing: the line is
      looked up within the purchase's own lines.
- [ ] AC10: the route is gated `inventory.write`: a principal holding only `purchases.create`
      gets the forbidden page and the product's cost is untouched.
- [ ] AC11: a non-draft purchase refuses the action and leaves the product untouched — the
      handler holds the draft check, not just the template.
- [ ] AC12: the badge and the warning are pure reads: the only write in the change is the
      apply-cost action, and the purchase flow still never writes `products.cost_price` (the
      pre-existing AC10 test passes unchanged).

## Verification

- `src/routes/inventory_web.rs`: the badge — appears with both values, hides when equal, hides
  without supplier rows, hides when the stored cost is zero, and uses the preferred supplier's
  cost rather than the cheapest (`web_product_detail_stale_cost_badge_*`).
- `src/services/purchases.rs`: the line flag — strict inequality, non-zero stored gate, mixed
  draft flags only qualifying lines (`stale_line_cost_*`).
- `src/routes/purchases_web.rs`: the action — writes the line cost and recomputes the derived
  price (`web_apply_line_cost_writes_the_line_cost_into_the_product`,
  `web_apply_line_cost_recomputes_a_markup_derived_sale_price`), empty-body / injected-field
  rejection (`web_apply_line_cost_ignores_a_client_supplied_cost`), cross-purchase line is a 404
  (`web_apply_line_cost_refuses_a_line_from_another_purchase_and_leaves_both_products_untouched`),
  permission refusal leaves the product untouched
  (`web_apply_line_cost_refuses_a_principal_without_inventory_write`), non-draft refusal
  leaves the product untouched.
- `src/smoke_tests.rs`: the wiring guard still renders every seeded page — the new route is
  NOT probed by the guard (gap, see the design).
- `e2e/tests/test_products.py`: browser tests for the badge with both values and its hiding.
- `e2e/tests/test_purchases.py`: the browser journey — warn, apply, product and derived price
  updated, no badge after a fresh cost.
