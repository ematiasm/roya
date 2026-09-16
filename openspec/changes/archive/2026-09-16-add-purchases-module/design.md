# Design: add-purchases-module

## Architecture (mirror orchestrator of M2)
```
routes/purchases_api.rs + purchases_web.rs -> PurchasesService
  -> PurchaseRepository + SupplierRepository + ProductSupplierCostRepository + DocSequenceRepository
  -> InventoryService (In reason Purchase, Out reason Purchase-return)
  -> TransactionService (Expense per payment, Income per refund)
  -> PaymentMethodService (allowlist check, reused from M0)
```
- M3 never SQLs `transactions`, `stock_movements`, `accounts`, or `payment_methods` for writes.
- Same pre-validate -> sequence -> stock -> finance -> document ordering as M2, with the
  same documented limitation (no shared SQLite tx across services; a late failure can skip
  a document number). A new document `StockMovement` reason `Purchase-return` is the only
  touch on M1, and only as a CHECK expansion.

## Migrations
1. `create_suppliers` — table + `UNIQUE(name)` + index on `is_active`.
2. `create_product_supplier_costs` — table + `UNIQUE(product_id, supplier_id)` +
   indexes `(product_id)`, `(supplier_id)` + partial unique index enforcing one
   preferred supplier per product.
3. `create_purchases` — table + `UNIQUE(purchase_number)` + CHECKs + indexes
   `(status)`, `(supplier_id)`, `(purchase_date)`.
4. `create_purchase_lines` — CASCADE purchase, RESTRICT product.
5. `create_purchase_payments` — CASCADE purchase, RESTRICT account, RESTRICT method.
6. `expand_stock_reason_purchase_return` — rebuild `stock_movements` CHECK to include
   `Purchase-return`, preserving rows (same shape as M2's `Sale-return` migration).

## Key decisions and tradeoffs
| Option | Chosen | Why / cost |
|---|---|---|
| Pedido as its own table vs Draft purchase | Draft of the purchase | One document, one state machine, no idempotent conversion step; cost: a Draft is not a sent order, so "already ordered" is a note not a state |
| Partial receiving | Out of scope | User confirmed goods arrive together; cost: a real partial delivery forces manual adjustment until added |
| Cost table `product_supplier_costs` as satellite | Yes | N-to-N product/supplier cannot live in `products`; also answers "who sells this"; no duplication because `products` keeps no supplier reference |
| Cost rule | Option A: always shift current -> previous, store dates, derive the alert | A reference price that only rises makes every future order overpriced and breaks the feedback loop; cost: one extra pair of columns instead of a boolean flag |
| Price history table | Rejected | `purchase_lines` already stores product + cost + date + supplier through the header, so the series is queryable; a second store would be two truths |
| `products.cost_price` | Kept as fallback, never written by purchases | Services and one-off buys have no supplier row; removing the column is migration + service + route + template work for no gain today; read rule: satellite wins when rows exist |
| Delete vs deactivate supplier/product | RESTRICT + `is_active` | History must survive; cost: deletes need a deactivate step first |
| Cancel-refund balance guard | None needed | A purchase refund is money entering the account, unlike M2's refund which is money leaving |
| `PaymentMethod` catalog | Reused from M0 | Methods are a money concept shared by sales, purchases and future expenses; duplicating them per module would fork the catalog |

## Void vs return (deferred by design)
Annulling is a **correction**, not a new event: the purchase never should have happened,
the original keeps its number and is marked void, so no new numbering is needed.
Returning goods is a **new event** where the original purchase still stands (you bought
100, returned 10 defective) and therefore needs its own document.

In v1 only full annulment is possible because partial receiving is explicitly out of
scope, so `Confirmed -> Cancelled` covers it and no `RET` document exists. When partial
returns become real, `RET` is additive: one new table pair and an `INSERT` into
`doc_sequences` for `('RET', year, 0)`, with no migration of existing data. Stock keeps
using reason `Purchase-return` and is distinguished by `reference` (`PURCH` today, `RET`
number later).

Hard rule for that future work: design partial returns for **sales and purchases together**.
Doing only the purchase side would leave sales annulling by transition while purchases emit
documents, an asymmetry that later needs refactoring.

## HTMX parity
- `/purchases` page: purchase list with status/payable badges, Draft line editor,
  Confirm/Cancel buttons, Pay form for Credit, plus a "Sugerido" panel that renders the
  pedido suggestion and pre-fills a Draft. Fragments mirror `/products` and `/sales`.

## Interaction with existing modules
- M1 `InventoryService` gains no new business rule: only the reason value is new.
- M0 gains no new concept: `payment_methods` and the allowlist are consumed as-is.
- M2 is untouched; sales and purchases never reference each other.
