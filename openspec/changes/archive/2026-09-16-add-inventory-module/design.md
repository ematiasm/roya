# Design: add-inventory-module

## Architecture (mirror M0)
```
handler (routes/inventory.rs) -> service (services/inventory.rs)
  -> repository traits (repositories/{category,product,barcode,stock}_repo.rs) -> sqlite
```
- Same stack: Axum + SQLx SQLite + Askama + HTMX + rust_decimal-as-TEXT.
- New traits `CategoryRepository`, `ProductRepository`, `BarcodeRepository`,
  `StockMovementRepository` with SQLite impls. Services depend only on traits
  (Postgres swap later = new impls, no handler/service rewrite).
- `AppState` extends with inventory services; existing finance wiring untouched.

## Migrations (new files, embedded via `sqlx::migrate!`)
1. `create_categories` — table + self-FK + `UNIQUE(parent_id, name)` + index on `parent_id`.
   Note: SQLite treats NULLs as distinct in UNIQUE, so duplicate root names need
   a service-level check (documented, tested).
2. `create_products` — table + FK `category_id SET NULL` + `UNIQUE(sku)` +
   `CHECK(kind IN ...)`.
3. `create_product_barcodes` — table + FK CASCADE + `UNIQUE(code)`.
4. `create_stock_movements` — table + FK RESTRICT + CHECKs + indexes
   `(product_id)`, `(product_id, date)`.

## Key decisions
- Decimal-as-TEXT for `sale_price/cost_price/qty/min/max`: same as finance
  (SQLite NUMERIC->REAL precision loss). Sums in Rust.
- Category cycle guard in service (recursive ancestor walk, depth-bounded);
  DB CHECK covers only self-parent.
- `stock_movements.product_id ON DELETE RESTRICT`: history is preserved;
  product deactivation (`is_active=0`) preferred over delete. Barcodes CASCADE
  because they are aliases, not history.
- Permissive negatives default: no compensating transaction needed; negative
  stock is queryable state, corrected by later `Adjust`/`In`.

## Tradeoffs
| Option | Chosen | Why / cost |
|---|---|---|
| Single DB + logical boundary vs DB per module | Single DB | Local single-user, keeps ACID + backups simple; cost: discipline needed to avoid cross SQL (enforced by repo traits + review) |
| Cycle check DB vs service | Service walk + self-CHECK | SQLite has no recursive CHECK; cost: two-layer validation |
| UNIQUE root names DB vs service | Service check | SQLite NULL-distinct semantics; cost: race covered by re-check on conflict |
| Quantity as Decimal vs Integer | Decimal-as-TEXT | Supports kg/lt fractions; cost: Rust-side sums |
| Auto finance posting on Purchase/Sale reason | Rejected | Would couple M0<->M1 without M2 orchestrator; cost: double manual load until M2 |
| `is_sellable/tax/supplier` columns now | Deferred | Avoid bloat/speculation; cost: future migration (accepted by user) |

## HTMX parity
- `/products` page: product table + low/negative stock badges, category filter,
  barcode search box; forms mirror dashboard partials; `HX-Trigger` refresh.
