# Tasks: add-sales-module

## Review Workload Forecast
- Estimated: ~1000-1200 lines (4 migrations + models + 2 repos + service + 2 routes + templates + tests).
- Chained PRs recommended: Yes (2 slices: C domain/orchestrator, D routes/UI).
- 400-line risk: High single PR -> 2 PRs.
- Decision needed before apply: Yes.

## Slice C — domain + orchestrator (no routes)
- [x] T1: migrations doc_sequences/sales/lines/payments
- [x] T2: models Sale/SaleLine/SalePayment/DocSequence + DTOs
- [x] T3: SaleRepository + DocSequenceRepository traits + SQLite impls
- [x] T4: SalesService (Draft/Confirm/Pay/Cancel, number gen, freeze, orchestrated stock+finance calls)
- [x] T5: unit tests AC1-AC7 service-level, `cargo test`

## Slice D — routes + UI
- [x] T6: REST `/api/sales`, lines, payments, confirm/cancel endpoints
- [x] T7: Web `/sales` Askama + HTMX parity
- [x] T8: integration AC8-AC10 + finance/stock-via-services check
- [x] T9: README/env docs (no new env; document flows)
