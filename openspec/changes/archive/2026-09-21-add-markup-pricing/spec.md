# Spec: add-markup-pricing (M1 inventory)

Delivered on `feat/product-markup-pricing`; these rules are the present-tense truth in
`openspec/specs/inventory/spec.md` (promoted into its entity list and Rules section). This delta
extends the existing inventory capability; no existing rule was changed or removed.

## Entity field (added to `products`)

- `markup_pct` TEXT NULL. The percentage applied over `cost_price` to derive the sale price.
  `NULL` means "no markup, the price is manual" and is deliberately NOT the same as `0` (a 0%
  markup would pin the price to the cost). Nullable, not defaulted: no backfill was run and no
  stored price changed on migration.

## Rules

- **A set markup derives the sale price from the cost.** `sale_price = cost_price *
  (1 + markup_pct/100)`, computed server-side inside `InventoryService::validate_product`, the
  single validation entry for both create and update. When a markup is set the submitted
  `sale_price` is ignored.
- **The stored price is authoritative and never contradicts its parts.** The derivation runs at
  write time, so at rest a product with a markup always holds exactly the derived price —
  recomputing from the stored `cost_price` and `markup_pct` yields the stored `sale_price`. This
  is a service-level invariant, not a database CHECK (SQLite cannot do decimal arithmetic on TEXT
  columns); a write bypassing `InventoryService` could break it. No historical document depends on
  it: `sale_lines.unit_price` snapshots the price when a line is built.
- **The price rule applies to the effective price.** `sale_price > 0` for `Product` and `>= 0` for
  `Service` are checked against the derived price when a markup is set, the submitted price
  otherwise — a caller that supplies a markup is not also required to supply a meaningful price.
- **Guards on the markup itself.** `markup_pct <= -100` is rejected; a set markup with
  `cost_price <= 0` is rejected, so a product can never be silently derived to a free price.
  Deliberate divergence, consistent with the pre-existing rule: a derived `0.00` is refused for a
  `Product` but accepted for a `Service`, which may be free.
- **Clearing the markup keeps the last stored price** and returns the product to a manual price.
  On the API the field is clearable via `double_option` like `location`/`notes`: absent leaves the
  stored markup (and its derived price) unchanged, explicit `null` clears.
- **The derived price is pinned to cents with half-up rounding.** `MidpointAwayFromZero`
  (10.005 → 10.01), applied only to the derived price; manual prices are stored exactly as sent.
  The percentage is applied as a multiplication by `Decimal::new(1, 2)` — the project never
  divides a `Decimal`.
- **A malformed stored `markup_pct` degrades to "no markup, manual price"**, never to 0%
  (`parse_decimal_opt_strict`; the loose parser maps malformed values to `Decimal::ZERO`, which
  for a markup would silently pin the price to the cost).
- **Display never misstates the stored price.** `money_display` normalises a stored value at scale
  ≤ 2 up to exactly two decimals; anything finer prints as stored, never rounded — showing 7.78
  for a stored 7.777 would misstate what the customer is charged.

## Interface

- No new routes. `POST /api/products` accepts an optional `markup_pct`; `PUT /api/products/{id}`
  accepts it clearable via `double_option`.
- Web: both the create modal and the product drawer gained a "Markup %" input. In the product
  drawer, when a product has a markup the price input renders `readonly` server-side showing the
  stored price, with a hint that it is recalculated on save; in the create modal there is no
  stored product, so the price input's `readonly` state is toggled by a small client-side script,
  with no derived value and no hint. The `readonly` attribute is courtesy only — the handler is
  the enforcement. Prices render through `money_display` (at-or-below-2 scale shown at exactly two
  decimals).

## Acceptance criteria

- [ ] AC1: create and update with `markup_pct` set store `sale_price = cost_price *
      (1 + markup_pct/100)`, pinned to cents with half-up rounding, and ignore the submitted
      price.
- [ ] AC2: the rounding strategy is half-up at exact midpoints — a derived 10.005 stores 10.01,
      which the non-midpoint tests alone cannot prove.
- [ ] AC3: `markup_pct <= -100` is rejected, and a set markup with `cost_price <= 0` is rejected.
- [ ] AC4: the price rule applies to the effective price: a caller supplying a markup may send a
      meaningless `sale_price`; a derived `0.00` is refused for a `Product` and accepted for a
      `Service`.
- [ ] AC5: at-rest invariant: after every accepted write with a markup set, the stored price
      equals the price derived from the stored `cost_price` and `markup_pct`.
- [ ] AC6: a malformed stored `markup_pct` reads back as `NULL` (manual price), never as 0%.
- [ ] AC7: clearing the markup (explicit `null` on update) keeps the last stored price and
      returns the product to a manual price; an absent key leaves the markup unchanged; patching
      only `cost_price` re-derives the price.
- [ ] AC8: manual prices (no markup) are stored verbatim and never rounded — a stored 7.777 stays
      7.777 and displays as 7.777, never 7.78.
- [ ] AC9: both web forms render the "Markup %" input; with a markup present the price input is
      `readonly` showing the last stored derived price, and saving re-derives the price from the
      cost even when the browser re-submits the stale readonly value (first smoke coverage of a
      `/web/products/edit` post).
- [ ] AC10: history is unaffected: a sale line prices from the product's stored price at line
      build (or an explicit override) and stores that `unit_price`; later markup changes do not
      move existing lines.

## Verification

- `src/services/inventory.rs`: derivation, guards, midpoint rounding test (`10.005 → 10.01`),
  effective-price cases, manual-price exactness, clear-keeps-price, patching cost re-derives
  (`patching_cost_price_recomputes_the_derived_price`), product-vs-service divergence, and the
  overflow hardening (`an_overflowing_markup_derivation_is_a_validation_error_not_a_panic`,
  `an_overflowing_cost_derivation_is_a_validation_error_not_a_panic`).
- `src/routes/inventory_api.rs`: REST round-trip of `markup_pct` on create and update, `null`
  clears, an absent key leaves the markup unchanged.
- `src/repositories/product_repo.rs`: a malformed stored `markup_pct` reads back as "no markup",
  not 0% (AC6, `malformed_stored_markup_pct_reads_back_as_none_not_zero`).
- `src/services/sales.rs`: history is unaffected —
  `a_sale_line_keeps_the_price_it_snapshotted_when_the_products_markup_moves` asserts both halves
  (the product's price moved, the sale line's did not).
- `src/smoke_tests.rs`: the first smoke post to `/web/products/edit` — save with the stale
  readonly price, save with a changed markup.
- `e2e/tests/test_products.py`: five browser tests over the drawer flow — the original three plus
  the two reset/editability regressions added by `e76882a`
  (`test_second_create_after_a_markup_create_is_not_poisoned_by_the_reset`,
  `test_drawer_markup_field_toggles_the_price_editability_live`).
