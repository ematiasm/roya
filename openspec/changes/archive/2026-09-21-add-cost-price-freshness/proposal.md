# Proposal: add-cost-price-freshness (M1 inventory / M3 purchases)

## Status and provenance

Delivered on `feat/cost-price-freshness` and promoted into `openspec/specs/inventory/spec.md`
and `openspec/specs/purchases/spec.md`; this folder is the change record, authored directly in
its archive home. It is the sibling of `2026-09-21-add-markup-pricing` — that change derived
prices from `cost_price`; this one surfaces when `cost_price` itself is stale. It modifies two
existing capabilities: inventory (the product drawer) and purchases (the draft line editor).

## Problem statement

`products.cost_price` is a column only a human ever writes
(`migrations/20240101000004_create_products.sql:11`). Confirming a purchase never touches it:
the confirmation updates the per-supplier satellite `product_supplier_costs` instead, a rule
the purchases spec states ("a purchase never writes `products.cost_price`") and a test pins
(`src/services/purchases.rs:2259`,
`ac10_purchase_never_writes_cost_price_and_satellite_wins`). The column's default is `'0'`
(`NOT NULL DEFAULT '0'`), so a fresh product carries a zero that means "no cost recorded yet",
not a cost.

The consequence: the stored cost drifts behind what suppliers actually charge. Nothing in the
system shows the drift, and the markup change made silence expensive — that change derives
`sale_price = cost_price * (1 + markup_pct/100)` inside `InventoryService::validate_product`,
so a stale cost now silently produces a stale derived price. The operator buying at a new
price has no signal that the product's stored cost — and therefore its derived price — is
behind reality.

## Goal

Surface the drift with two complementary signals, both **derived and never stored**:

1. **A permanent badge** in the product drawer's supplier-costs card: "stale cost", showing
   `reference $X • stored $Y`, when `SupplierService::reference_cost(product_id)` — the
   preferred supplier's current cost, else the cheapest, else nothing — disagrees with the
   stored cost. Three gates keep the badge honest: a reference cost must exist (with no
   supplier rows the column IS the truth), the stored cost must not be zero (zero means "no
   cost recorded yet", not a cost to compare against), and the two must actually differ.
2. **An ephemeral warning and an action** on a draft purchase line whose cost is HIGHER than
   the product's stored cost: the warning shows both numbers, and an "Apply to product" button
   posts `POST /web/purchases/{purchase_id}/lines/{line_id}/apply-cost`, which writes the
   line's cost into the product through `InventoryService::update_product` with a patch
   carrying only `cost_price`, so a markup-derived `sale_price` is recomputed by the existing
   derivation.

Neither signal writes anything: the badge is a read-time comparison, the warning is a
read-time flag on the purchase detail view, and the apply-cost action is a separate,
human-triggered product write through the inventory service. The purchase flow itself still
never writes the column — AC10 and its test are untouched.

## What this change owns

- `SupplierService::reference_cost` reused as-is for the badge's reference read
  (`src/routes/inventory_web.rs:511`); the badge is a `StaleCostView` passed to the product
  drawer partial (`templates/partials/product_detail.html`).
- A `stale_cost` field on the purchase line view (`src/services/purchases.rs:440`,
  `src/models.rs:1124`), derived from the product read the view already fetches, and rendered
  as a sub-row under the draft line (`templates/partials/purchase_detail.html`).
- The `web_apply_line_cost` handler and its route (`src/routes/purchases_web.rs:749`,
  `:1030`), gated `Require<InventoryWrite>` and draft-only.
- Browser tests for the badge (`e2e/tests/test_products.py`) and for the apply-cost journey
  (`e2e/tests/test_purchases.py`).

## Rules

1. **Nothing is stored.** Both signals are computed at read time; the only write in this
   change is the human-triggered apply-cost action, which writes a product through the
   inventory service exactly as any product edit would.
2. **The badge gates.** It appears only when a reference cost exists, the stored cost is not
   zero, and the two differ. Equal costs are fresh, not stale.
3. **The warning is strict and real.** It fires only when the line cost is strictly higher
   than a non-zero stored cost. Equal or lower is not what it warns about — the drawer badge
   covers any other disagreement.
4. **The request body is never read.** The handler takes no body extractor: the product and
   the cost are resolved from the stored line, found within the purchase's own lines.
5. **Draft-only.** The button renders only on a draft, and the handler refuses a non-draft
   purchase itself.
6. **Permission follows the effect.** The action writes a product, so it is gated
   `inventory.write`, not `purchases.create`.

## Out of scope

- Syncing `cost_price` automatically from the satellite inside `record_cost` or at purchase
  confirmation. Rejected: it would break AC10 and change what the purchases spec promises.
  See the design's rejected alternatives.
- Making `cost_price` itself derived at read time. Rejected: it stays a stored,
  human-approved column.
- Extending the smoke suite's generic form-wiring guard to the new route. The guard's shared
  fixture builds a purchase line whose cost is below the product's stored one, so the warning
  never renders on a guarded page and the new `hx-post` is never probed there. The route is
  covered by its own route tests and a browser journey instead; changing the shared fixture
  was deliberately declined as too risky for the benefit. Recorded as a gap in the design.
- Sharing the draft-status guard between the handler and the service, and de-duplicating the
  supplier-cost query on the drawer. Both are small follow-ups recorded in the design's
  deferred items.

## Known impact

- `openspec/specs/inventory/spec.md` gains the badge as a derived read in the products rules.
- `openspec/specs/purchases/spec.md` gains the apply-cost route in its Interface section and a
  third deliberate authorization consequence (the route lives on the purchases page but is
  gated `inventory.write` because its effect lands on the product).
- The duplicate supplier-cost read on the drawer (rows + `reference_cost`) is a known
  non-hot-path inefficiency, recorded as a follow-up, not fixed here.

## Acceptance summary

The product drawer shows the badge exactly when a supplier reference cost disagrees with a
non-zero stored cost; a draft line warns only when its cost is strictly above a real stored
cost; "Apply to product" writes the line's cost into the product through the inventory
service, recomputing a markup-derived sale price, for a draft, from the stored line, with a
body that is never read, under `inventory.write`; a principal who can read the purchase page but
lacks that permission sees a visible refusal (on the HTMX click, the application's global error
notice, not the forbidden page); the purchase flow still never writes the column. Covered by the Rust
suite (route tests for both signals, the service-level flag tests, the permission and
cross-purchase refusals), the smoke suite, and browser tests for the badge and the journey.
