# Tasks: add-purchases-module

## Review Workload Forecast
- Estimated: ~1300-1600 lines total (6 migrations + models + 4 repositories + service +
  2 route files + templates + tests). Larger than M2 because M3 adds two entities
  (suppliers, cost satellite) plus the suggestion builder.
- Chained PRs recommended: Yes — 3 slices.
- 400-line budget risk: High for a single PR.
- Decision needed before apply: Yes.

## Slice E — suppliers + cost satellite (no purchases yet)
- [x] T1: migrations `create_suppliers`, `create_product_supplier_costs`
- [x] T2: models `Supplier`, `ProductSupplierCost` + DTOs
- [x] T3: `SupplierRepository` + `ProductSupplierCostRepository` traits and SQLite impls
- [x] T4: `SupplierService` (CRUD, deactivate, RESTRICT delete) and cost recording rule
      (shift current -> previous with dates)
- [x] T5: tests AC9 partially, AC10, AC13 at service level

## Slice F — purchases domain + orchestrator
- [x] T6: migrations `create_purchases`, `create_purchase_lines`, `create_purchase_payments`,
      `expand_stock_reason_purchase_return`
- [x] T7: models `Purchase`, `PurchaseLine`, `PurchasePayment` + DTOs
- [x] T8: `PurchaseRepository` trait + SQLite impl (`PURCH` sequence reuse)
- [x] T9: `PurchasesService` (Draft/Confirm/Pay/Cancel, orchestrated stock + finance,
      satellite update on confirm)
- [x] T10: suggestion builder (low-stock + chosen supplier + satellite cost +
      `without_supplier` list)
- [x] T11: service tests AC1-AC7, AC9-AC12

## Slice G — routes + UI
- [ ] T12: REST `/api/suppliers`, `/api/product-supplier-costs`, `/api/purchases`,
      `/api/purchases/suggestions`
- [ ] T13: Web `/purchases` Askama + HTMX parity, including the suggestion panel
- [ ] T14: route-level tests AC8, AC11, AC12, AC14
- [ ] T15: README update (flows, migrations, endpoints)

## Verify
- [ ] `cargo test` full suite green; manual smoke: seed supplier, register cost, build
      pedido from suggestion, confirm Cash and Credit, cancel both paths
