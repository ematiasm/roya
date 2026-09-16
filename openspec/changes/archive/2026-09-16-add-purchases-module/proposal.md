# Proposal: add-purchases-module (M3 Compras + Proveedores)

## SDD Session Preflight (reused)
- execution: auto, store: openspec, delivery: ask-on-risk, budget: 400, strict_tdd: true

## Problem statement
Money leaving for merchandise is manual in M0 and stock entry is manual in M1,
with no link between them. There is no supplier record, no per-supplier cost,
and no way to turn the M1 `min_stock`/`max_stock` suggestion into an actual order.
A polirrubro buys the same product from different suppliers at different prices,
and that knowledge currently lives nowhere.

## Goal
M3 Compras as the mirror orchestrator of M2, plus two supporting entities:

- `suppliers` — real table (unlike customers, suppliers get orders and history).
- `product_supplier_costs` — satellite of the product/supplier relation holding the
  per-supplier price with its previous value. This single table answers both
  "what does each supplier charge me" and "who do I buy this from", which is what
  the purchase-order builder consumes.
- `purchases` / `purchase_lines` / `purchase_payments` — with `doc_sequences`
  consumer `PURCH`, so numbering is `YYYY-PURCH-NNNNNN`.

The Draft state of a purchase **is** the pedido: it is seeded from the low-stock
suggestion, can be edited, and only touches stock and finance when confirmed.

## Flows
- Suggest: derive low-stock products, pick the supplier (preferred, else cheapest
  current cost), propose `max_stock - stock`, pull `unit_cost` from the satellite.
- Draft: create/edit lines freely. No stock, no finance, no cost update.
- Confirm: assign `purchase_number`, freeze line costs, stock `In` reason `Purchase`
  for each tracked line, then Cash => 1 payment + 1 Expense; Credit => payable, no
  Expense until paid. Reception is complete: no partial receiving in v1.
- Pay (Credit): each payment creates an Expense; Paid when `SUM(payments) >= total`.
- Cancel: from Draft => discard, no side effects. From Confirmed => goods go back to
  the supplier (stock `Out` reason `Purchase-return`) and money already paid comes back
  as Income per payment. A refund is money *entering*, so unlike M2's refund it cannot
  overdraft and needs no balance guard.

## Cost rule (frozen, option A)
On confirm, for each line: if the line cost differs from the satellite's current cost,
the current becomes `previous` with its date, and the new value becomes current. The
"supplier raised the price" alert is **derived** by comparing previous vs current, never
stored. `products.cost_price` is NOT written by a purchase: it remains the fallback for
products with no supplier row (services, one-off buys). Read rule: if the satellite has
rows for the product, the satellite wins; otherwise the column.

## Included by symmetry (veto if you disagree)
Confirmed -> Cancelled, because "compré mal, lo devuelvo" is real life. It needs one
new movement reason `Purchase-return`, which costs a CHECK expansion migration exactly
like M2's `Sale-return`.

## Non-goals
No partial receiving, no multi-warehouse, no returns-to-supplier as their own document,
no FIFO or weighted-average valuation, no payment scheduling or due-date reminders,
no supplier tax id, no AFIP, no automatic reordering.

## Acceptance summary
Draft touches nothing; Confirm numbers + receives stock + posts cost per supplier;
Credit payable derived; Cancel reverses both sides; suggestion endpoint builds the pedido.
