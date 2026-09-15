# Proposal: add-inventory-module (M1 Stock e Inventario)

## SDD Session Preflight
- execution: auto
- artifact_store: openspec
- delivery_strategy: ask-on-risk
- review_budget: 400
- strict_tdd: true (`cargo test`)
- confirmed: 2026-09-14, user said "dale"

## Problem statement
Roya today is only M0 Finanzas (`accounts` + `transactions`, derived balance).
For a polirubro (comercio + servicios) money enters via product/service sales and
leaves via compras, alquiler, servicios, mantenimiento, salarios, inversiones.
Without inventory there is no way to know stock, low stock, or what to reorder.
Manual finance entries cannot answer "¿qué me falta comprar?".

## Goal
Add M1 Inventario as an independent but interrelated module (systems theory:
suprasistema Negocio -> subsistemas Finanzas + Inventario). M1 is autonomous,
shares one SQLite DB with M0 but with hard logical boundary (no cross SQL).
Future M2 Ventas / M3 Compras-Pedidos will orchestrate M0+M1 via domain events,
not direct table writes.

## Scope (frozen v1)
Entities (DB + code in English, UI in Spanish):

- `categories`: `id, name, parent_id -> categories.id NULL`
  Free, self-referencing. Root = `parent_id IS NULL`.
- `products`: `id, sku UNIQUE, name, kind [Product|Service], category_id NULL,
  unit (simple text), sale_price, cost_price, track_stock BOOL,
  min_stock NULL, max_stock NULL, location NULL, notes NULL,
  is_active BOOL, created_at, updated_at`
  Only added `location, notes, updated_at` as reservation. Rest deferred to migrations.
- `product_barcodes`: `id, product_id -> products ON DELETE CASCADE, code UNIQUE`
  0..N per product.
- `stock_movements`: `id, product_id, qty (always > 0 input, signed by type),
  type [In|Out|Adjust], reason [Purchase|Sale|Loss|Adjust|Initial],
  reference (free text, future sale/purchase id), date, created_at`
  `stock = SUM(In) - SUM(Out) +/- Adjust`, derived like finance balance.

## Rules / invariants
1. `kind=Service` => `track_stock=false`, `min/max=NULL`, rejects movements.
2. `track_stock=true` => `min_stock >= 0`, `max_stock >= min_stock`.
3. `UNIQUE(parent_id, name)` on categories; no self/descendant cycles;
   cannot delete category with children or products.
4. `UNIQUE(sku)` on products; `UNIQUE(code)` on barcodes.
5. `ALLOW_NEGATIVE_STOCK=true` by default (permissive, frictionless daily ops).
   `false` blocks like finance. Negative = alert, not silent.
   Reorder suggestion derived: `if current <= min => suggested = max - current`.
6. Boundary: M1 never writes `transactions`; M0 never writes `stock_*`.
   Single DB `roya.db`, logical separation via service/repository traits only.

## Non-goals
No suppliers FK, no `is_sellable/is_purchasable`, no `tax_rate`, no lots/expiry,
no FIFO/average costing, no multi-branch, no images/JSON metadata,
no automatic finance postings. Pure-finance outflows (alquiler, salarios, etc.)
stay manual in M0.

## Acceptance (summary, full criteria in spec.md)
1. Cannot create movement for Service.
2. Cannot load min/max on Service.
3. Cannot duplicate sku / barcode.
4. Cannot create category cycle; cannot delete non-empty category.
5. With flag true can go negative (flagged); with false blocks.
6. Stock equals sum of movements; suggestion = max - current when <= min.
7. Deleting/deactivating product never touches finance tables.

## Risks
- Permissive negatives hide miscounts -> mitigated by negative/low-stock list.
- Simple `unit` text (1kg != 1000g) -> explicit, conversions are out of scope.
- Category trees can grow messy -> move/rename instead of delete.
