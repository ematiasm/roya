# Proposal: add-sales-module (M2 Ventas)

## SDD Session Preflight (reused)
- execution: auto, store: openspec, delivery: ask-on-risk, budget: 400, strict_tdd: true

## Problem statement
M0 Finanzas and M1 Inventario are disconnected. Sales of products/services
(polirubro, contado + credito) are loaded twice by hand, with no trace
sale -> stock -> money. No pending-debt view, no return flow.

## Goal
M2 Ventas as orchestrator (Odoo-style independent documents in one flow).
Independent tables `sales/sale_lines/sale_payments` + universal `doc_sequences`.
Calls `InventoryService` (Out/In reason Sale/Return) and `TransactionService`
(Income per payment / Expense refund). Never writes finance/stock tables directly.

## Scope frozen v1
- `doc_sequences(doc_type TEXT, year INTEGER, last_number INTEGER,
  PK(doc_type, year))`. First consumer: `('SALE', year)`.
- `sales`: `id, sale_number UNIQUE (YYYY-SALE-SEQ, assigned on confirm),
  status [Draft|Confirmed|Cancelled], payment_type [Cash|Credit],
  customer_name TEXT (transitional, until Clientes), sale_date, due_date NULL
  (Credit only), receipt_no NULL UNIQUE, notes NULL, cancel_reason NULL,
  created_at, updated_at, confirmed_at NULL, cancelled_at NULL`.
  No sale-level account_id: each payment carries its account.
  Total, paid, due derived, never stored as truth.
- `sale_lines`: `sale_id CASCADE, product_id RESTRICT, qty > 0,
  unit_price frozen at confirm, subtotal derived`.
- `sale_payments`: `sale_id CASCADE, account_id (M0) NOT NULL,
  amount > 0, date, created_at`. Each row generates one M0 Income with
  reference = sale_number.

## Flows
- Draft: create lines, no stock, no finance. Editable.
- Confirm: assign sale_number in tx; freeze prices; Out movements reason=Sale
  (respects ALLOW_NEGATIVE_STOCK); Cash => 1 payment+Income now;
  Credit => receivable, no Income yet.
- Pay (Credit): each payment => Income; Paid when SUM(payments) >= total.
- Cancel/Return: only from Confirmed; In reason=Sale-return (product came back);
  refund Expense if money was received (respects ALLOW_NEGATIVE_BALANCE).

## Non-goals
No customer FK, no discounts, no tax calc, no cash-register closing,
no sellers, no AFIP, no partial stock reservation on Draft.

## Acceptance summary
Draft touches nothing; Confirm deducts + numbers + posts Cash income;
Credit debt derived; Annul re-enters + refunds guarded; finance/stock only via services.
