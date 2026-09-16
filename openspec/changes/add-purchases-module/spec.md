# Spec: add-purchases-module

## Entities

### suppliers
- `id PK`, `name TEXT NOT NULL UNIQUE` (trimmed, non-empty, <= 128)
- `phone TEXT NULL` (<= 32), `notes TEXT NULL` (<= 512)
- `is_active INTEGER NOT NULL DEFAULT 1`
- `created_at`, `updated_at`
- Rule: cannot delete a supplier referenced by purchases or cost rows (RESTRICT);
  deactivate with `is_active=0` instead.

### product_supplier_costs (satellite)
- `id PK`, `product_id NOT NULL -> products(id) ON DELETE RESTRICT`
- `supplier_id NOT NULL -> suppliers(id) ON DELETE RESTRICT`
- `current_cost TEXT NOT NULL` (Decimal >= 0, Decimal-as-TEXT)
- `current_cost_updated_at TEXT NOT NULL`
- `previous_cost TEXT NULL`, `previous_cost_updated_at TEXT NULL`
- `is_preferred INTEGER NOT NULL DEFAULT 0`
- `supplier_sku TEXT NULL` (the supplier's own code for this product, <= 64)
- `created_at`
- `UNIQUE(product_id, supplier_id)`
- Rules: at most one `is_preferred=1` per product; `previous_cost` is never newer
  than `current_cost_updated_at`.

### purchases
- `id PK`, `purchase_number TEXT UNIQUE NULL` (`YYYY-PURCH-NNNNNN`, zero-padded 6,
  assigned on confirm, immutable, NULL only while Draft)
- `supplier_id NOT NULL -> suppliers(id) ON DELETE RESTRICT`
- `status TEXT CHECK(status IN ('Draft','Confirmed','Cancelled'))`
- `payment_type TEXT CHECK(payment_type IN ('Cash','Credit'))`
- `purchase_date TEXT (NaiveDate)`, `due_date TEXT NULL` (required if Credit)
- `supplier_invoice_no TEXT NULL` (the supplier's document number, <= 64)
- `notes TEXT DEFAULT ''` (<= 512), `cancel_reason TEXT NULL`
- `created_at`, `updated_at`, `confirmed_at NULL`, `cancelled_at NULL`
- Derived, never stored: `total = SUM(qty*unit_cost)`, `paid = SUM(payments)`,
  `due = total - paid`, `payment_status`.

### purchase_lines
- `id PK`, `purchase_id NOT NULL -> purchases(id) ON DELETE CASCADE`
- `product_id NOT NULL -> products(id) ON DELETE RESTRICT`
- `qty TEXT` (Decimal > 0), `unit_cost TEXT` (Decimal >= 0, frozen at confirm,
  editable while Draft)
- `created_at`

### purchase_payments
- `id PK`, `purchase_id NOT NULL -> purchases(id) ON DELETE CASCADE`
- `account_id NOT NULL -> accounts(id) ON DELETE RESTRICT`
- `method_id NOT NULL -> payment_methods(id) ON DELETE RESTRICT`
- `amount TEXT` (Decimal > 0), `date TEXT`, `created_at`
- Rule: `(account_id, method_id)` must exist in `account_payment_methods` (M0
  allowlist), else 400. `SUM(payments) <= total`.

### stock_movements (M1, CHECK expansion only)
- Add reason `Purchase-return` to the allowed set (rebuild-preserving migration,
  same pattern as `Sale-return`).

## Derived reads
- Reference cost for a product: satellite current cost of the preferred supplier,
  else the lowest current cost, else `NULL` meaning fall back to `products.cost_price`.
- Price alert: `current_cost > previous_cost` => raised; `<` => lowered; else unchanged.
- Purchase suggestion (the pedido): product, suggested qty = `max_stock - current_stock`,
  chosen supplier, current cost, subtotal estimate. Products with no satellite row are
  returned in a separate `without_supplier` list, never silently dropped.

## State machine
- `Draft -> Confirmed`: assign number, freeze costs, stock `In` `Purchase` per tracked
  line, Cash => 1 payment + 1 Expense, Credit => payable only. Also updates the satellite
  cost for `(product, supplier)` per the frozen cost rule.
- `Draft -> Cancelled`: discard. No stock, no finance, no cost change.
- `Confirmed -> Cancelled`: stock `Out` `Purchase-return` per tracked line; Income refund
  per payment already made (money entering, no balance guard); `cancelled_at` + reason.
- No other transitions.

## Acceptance criteria
- [ ] AC1: Draft create/edit lines touches no stock, no finance, no satellite cost.
- [ ] AC2: Confirm Cash assigns `YYYY-PURCH-NNNNNN`, stock `In` `Purchase`, 1 payment, 1 Expense.
- [ ] AC3: Confirm Credit receives stock, creates zero Expense, `due = total`.
- [ ] AC4: Credit payments post Expenses; overpay => 400; Paid when `due = 0`.
- [ ] AC5: Unknown product/supplier/account => 404; `qty <= 0` or `unit_cost < 0` => 400.
- [ ] AC6: Double confirm => 400; any edit of a Confirmed purchase => 400.
- [ ] AC7: Cancel Confirmed returns stock (`Purchase-return`) and refunds paid amounts as Income
      per originating account; no balance guard can block it.
- [ ] AC8: `purchase_number` UNIQUE, immutable, NULL only in Draft.
- [ ] AC9: Confirm updates the satellite: previous <- current with its date, current <- line cost.
- [ ] AC10: A purchase never writes `products.cost_price`; read rule prefers the satellite
      when rows exist, otherwise falls back to the column.
- [ ] AC11: Finance and stock rows are written only through services, `reference` = `purchase_number`.
- [ ] AC12: Suggestion returns low-stock products with chosen supplier, `max_stock - stock` qty
      and satellite cost; no-supplier products appear in their own list.
- [ ] AC13: Deleting a supplier or product with history is blocked (RESTRICT).
- [ ] AC14: Payment with a `(account, method)` pair outside the M0 allowlist => 400 with no side effects.
