# Proposal: add-markup-pricing (M1 inventory)

## Status and provenance

Delivered on `feat/product-markup-pricing` and promoted into
`openspec/specs/inventory/spec.md`; this folder is the change record, authored directly in its
archive home. It modifies an existing capability — inventory — whose spec gains the pricing
rules this change delivered.

## Problem statement

`products.sale_price` and `products.cost_price` are two independent manual values
(`migrations/20240101000004_create_products.sql:10-11`), and nothing in the system relates them:
`InventoryService::validate_product` validates each on its own (non-negative cost, positive
price for a `Product`), with no rule connecting the two. There is no margin or markup field
anywhere — not in the schema, not in the model, not in the UI, not in the inventory spec, and not
in the sale logic, which prices a line from the product's stored `sale_price` or from an explicit
override. The consequence is that the markup is arithmetic the operator performs in their head —
"the cost is 80, I want 33% over cost, so… 106.66" — or skips entirely. A price set once and never
touched again drifts away from a cost that moved under it, and nothing in the system detects or
measures that drift. The shop owner's actual rule of thumb ("I sell at cost plus X") has no place
to live.

## Goal

Give a product an optional `markup_pct`. When it is set, the sale price is derived server-side as
`cost_price * (1 + markup_pct/100)` inside `InventoryService::validate_product`, the single
validation entry shared by create and update; when it is `NULL` the price is manual and behaves
exactly as before. The stored price remains the authoritative price — the derivation is a
convenience for setting it, not a live read-path computation.

## What this change owns

The `products.markup_pct` column (nullable TEXT, migration
`20240101000035_add_product_markup.sql`) and its model/repository plumbing; the derivation in
`InventoryService::validate_product` with its guards and rounding; `markup_pct` on the create and
update API bodies, clearable via `double_option` like `location`/`notes`; a "Markup %" input in
both the create modal and the product drawer, with the price input rendered `readonly` when a
markup is present; and `money_display` in `src/models.rs`, which normalises stored values that
already fit in two decimals up to exactly two so a list never prints `$100` beside `$100.00`.

## Rules

1. **`markup_pct` is nullable and `NULL` is a value, not an absence.** `NULL` means "no markup,
   the price is manual". It is deliberately not the same as `0`: a 0% markup would pin the price
   to the cost.
2. **When a markup is set the submitted `sale_price` is ignored**; the effective price is derived.
   The price rule (`sale_price > 0` for `Product`, `>= 0` for `Service`) is applied to the
   effective price, so a caller supplying a markup is not also required to supply a meaningful
   price.
3. **Clearing the markup keeps the last stored price** and returns the product to a manual price;
   nothing reverts to any earlier price.
4. **The guards**: `markup_pct <= -100` is rejected; a `cost_price <= 0` with a markup is
   rejected, so a product can never be silently derived to a free price.
5. **The derived value is pinned to cents, half-up** — the project's first rounding, confined to
   the derived price; manual prices stay exact.
6. **History is unaffected**: `sale_lines.unit_price` snapshots the price when a line is built
   (`src/services/sales.rs`) and never references the product.

## Out of scope

- Keeping `cost_price` fresh from the preferred supplier's cost — a separate change, planned in
  `odd/tasks/cost-price-freshness.md`. It matters because the markup is only as honest as the
  cost underneath it, but it has its own data-source question (which supplier cost, when).
- Normalising the money scale at the remaining supplier-cost display sites: the drawer's
  per-supplier cost rows and the purchase line unit cost. `money_display` exists after this
  change; adopting it at those two sites is a display-polish change with no behaviour in it.

## Known impact

- `openspec/specs/inventory/spec.md` gains the pricing rules and the `markup_pct` entity field
  when this change closes; until then it describes prices as purely manual.
- The derivation introduces the project's first `Decimal` rounding and its first percentage
  operation; both are confined to `validate_product` and pinned by tests (the design's "Rounding"
  section).
- A 35-line struct-literal churn ripples through the fixture code, adding `markup_pct: None`
  wherever a product literal is built; it is mechanical and carries no behaviour.

## Acceptance summary

A product with a markup holds exactly the price derived from its cost and markup, pinned to cents;
clearing the markup keeps the last price; the guards (`markup_pct > -100`, `cost_price > 0` when a
markup is set) hold; the effective-price rule spares the caller from sending a price alongside a
markup; malformed stored markup degrades to "no markup", never to 0%; the UI shows the markup and
reads back the last derived price; history prices are untouched. Covered by the Rust suite
(derivation, guards, rounding strategy, clearing semantics, API round-trip), the smoke suite
(first post to `/web/products/edit`) and three browser tests.
