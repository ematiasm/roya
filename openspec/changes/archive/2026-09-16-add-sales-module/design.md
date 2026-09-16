# Design: add-sales-module

## Architecture (orchestrator, Odoo-style)
```
routes/sales_api.rs + sales_web.rs -> SalesService
  -> SaleRepository + DocSequenceRepository (new traits)
  -> InventoryService (stock Out/In) + TransactionService (Income/Expense)
```
- M2 never SQLs `transactions` or `stock_movements` directly.
- SQLite tx per transition: number assignment + lines freeze + service calls
  committed atomically; on service error tx rolls back (single DB, no saga needed).
- `doc_sequences` row locked via `UPDATE ... SET last_number = last_number+1`
  in tx (single-user, no race).

## Migrations
1. `create_doc_sequences` — `(doc_type, year, last_number) PK(doc_type,year)`.
2. `create_sales` — table + `UNIQUE(sale_number)` + `UNIQUE(receipt_no)` +
   CHECKs + indexes `(status)`, `(sale_date)`.
   Partial uniqueness of sale_number with NULLs relies on SQLite NULL-distinct.
3. `create_sale_lines` — CASCADE sale FK, RESTRICT product FK.
4. `create_sale_payments` — CASCADE sale FK, RESTRICT account FK.

## Tradeoffs
| Option | Chosen | Why |
|---|---|---|
| Number on confirm vs create | Confirm | Avoids gaps from abandoned Drafts; matches ticket reality |
| One SALE series vs Cash/Credit split | One SALE series | TYPE = doc type, payment is attribute; fewer sequences, stable refs |
| Income per payment vs closing dump | Per payment | Traceability sale->money, credit support; closing becomes future read-view |
| Sale-level account vs per-payment | Per-payment only | Credit pays to different accounts over time; cash = single payment row |
| Paid as status vs derived | Derived | No drift; `due = total - paid` always true |
| Refund as Expense vs delete Income | Expense refund | Money left the account; delete would rewrite history; guard blocks overdraft |

## HTMX parity
- `/sales` page: sale list with status/debt badges, line editor in Draft,
  Confirm/Cancel buttons, Pay form for Credit, fragments like M1.
