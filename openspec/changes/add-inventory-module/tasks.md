# Tasks: add-inventory-module

## Review Workload Forecast
- Estimated changed lines: ~900-1100 (4 migrations + models + 4 repos + service + routes + templates + tests).
- Chained PRs recommended: Yes (2 slices: slice A DB+domain, slice B routes+UI).
- 400-line budget risk: High for single PR -> plan 2 PRs, each < 500 lines.
- Decision needed before apply: Yes — user approves this task list + slice plan.

## Slice A — DB + domain (no routes)
- [ ] T1: migrations (categories, products, barcodes, movements) + `sqlx migrate run` check
- [ ] T2: models (Category, Product, Barcode, StockMovement, DTOs) English names
- [ ] T3: repository traits + SQLite impls (TEXT decimal encode/decode, sums in Rust)
- [ ] T4: InventoryService (validations, cycle guard, root-name guard, negative-stock flag, derived stock/low/suggest)
- [ ] T5: unit tests (AC1-AC7 service-level, RED->GREEN, `cargo test`)

## Slice B — routes + UI
- [ ] T6: REST `/api/categories|products|stock-movements` + stock/low-stock endpoints, error mapping
- [ ] T7: Web `/products` Askama + HTMX fragments parity with dashboard
- [ ] T8: integration tests (AC8-AC11, finance untouched check)
- [ ] T9: `ALLOW_NEGATIVE_STOCK` wiring (env default true) + README update

## Verify
- [ ] `cargo test` full suite green, manual HTMX smoke (create product, In/Out, low-stock badge)
