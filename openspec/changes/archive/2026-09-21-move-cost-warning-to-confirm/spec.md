# Spec delta: move-cost-warning-to-confirm

## Changed — purchases Web interface

The stale-cost warning with its "Apply to product" action is gated on the purchase being
**Confirmed**, not draft. A draft line's cost is provisional; a confirmed line's cost is a fact.

## Rules

1. **Confirmed-only gate.** The stale-cost flag is derived in `PurchasesService::record_from_detail`
   only when the purchase status is `Confirmed`. The `web_apply_line_cost` handler refuses any
   other status itself, so the route stays safe even without the flag.
2. **Derivation gates kept.** The warning fires only when the line cost is **strictly higher**
   than the product's stored cost, and only when that stored cost is **non-zero** (zero means
   "no cost recorded yet", not a cost to compare against).
3. **A draft shows nothing.** A draft line is provisional and is never flagged.
4. **A cancelled purchase shows nothing.** A purchase confirmed and later cancelled is a
   historical document; it renders no warning and its costs must not feed a product update.
5. **AC10 untouched.** The action writes the line's cost into `products.cost_price` through
   `InventoryService::update_product` only — never through the purchase flow — and confirming a
   purchase still touches only the `product_supplier_costs` satellite.

## Acceptance criteria

- [ ] A line on a Confirmed purchase whose cost is strictly higher than the product's non-zero
      stored cost renders the stale-cost warning with both numbers and the "Apply to product"
      button.
- [ ] A line on a Draft purchase renders no warning, and the apply-cost handler refuses a draft
      purchase explicitly.
- [ ] A purchase confirmed and later cancelled renders no warning, and the handler refuses it.
- [ ] A line cost equal to or lower than the stored cost renders no warning (strictly-higher
      gate).
- [ ] A stored cost of zero renders no warning, whatever the line cost (non-zero gate).
- [ ] The action writes through `InventoryService::update_product` with a `cost_price`-only
      patch; `ac10_purchase_never_writes_cost_price_and_satellite_wins` passes unchanged.
- [ ] The warning sub-row spans the four columns a confirmed table renders (its remove column is
      draft-only), i.e. `colspan="4"` in `templates/partials/purchase_detail.html`.
- [ ] `cargo test` green (798 passed).
