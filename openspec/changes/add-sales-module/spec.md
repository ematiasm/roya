# Spec: add-sales-module

## Entities

### doc_sequences
- `doc_type TEXT`, `year INTEGER`, `last_number INTEGER NOT NULL DEFAULT 0`
- `PK(doc_type, year)`. Row `('SALE', YYYY)` created lazily on first confirm.

### sales
- `id PK`, `sale_number TEXT UNIQUE NULL` (NULL in Draft, set on confirm,
  format `YYYY-SALE-NNNNNN` zero-padded 6)
- `status TEXT CHECK(status IN ('Draft','Confirmed','Cancelled'))`
- `payment_type TEXT CHECK(payment_type IN ('Cash','Credit'))`
- `customer_name TEXT NOT NULL DEFAULT ''` (trimmed, <= 128)
- `sale_date TEXT (NaiveDate)`, `due_date TEXT NULL` (required if Credit, NULL if Cash)
- `receipt_no TEXT NULL UNIQUE`, `notes TEXT DEFAULT '' <= 512`
- `cancel_reason TEXT NULL`
- `created_at, updated_at, confirmed_at NULL, cancelled_at NULL`
- Rules: Draft editable (lines/customer/dates); Confirmed/Cancelled immutable
  except Cancel transition. `sale_number` immutable once set.

### sale_lines
- `id PK`, `sale_id NOT NULL -> sales(id) ON DELETE CASCADE`
- `product_id NOT NULL -> products(id) ON DELETE RESTRICT`
- `qty TEXT (Decimal > 0)`, `unit_price TEXT (Decimal >= 0, frozen copy of
  products.sale_price at confirm; editable in Draft)`
- `created_at`. Subtotal = qty * unit_price derived.

### sale_payments
- `id PK`, `sale_id NOT NULL -> sales(id) ON DELETE CASCADE`
- `account_id NOT NULL -> accounts(id) ON DELETE RESTRICT`
- `amount TEXT (Decimal > 0)`, `date TEXT (NaiveDate)`, `created_at`
- Rule: `SUM(payments) <= total` (overpay rejected).

## Derived
- `total(sale) = SUM(qty*unit_price)`; `paid = SUM(payments)`;
  `due = total - paid`; `payment_status = Paid if due <= 0 else Partial if paid > 0 else Unpaid`.

## State machine
- `Draft -> Confirmed` (assign number, freeze prices, stock Out Sale,
  Cash: 1 payment+Income). `Draft -> Cancelled` (no-op, no stock/finance).
- `Confirmed -> Cancelled` (stock In Sale-return for each Product line with
  track_stock, refund Expense per paid amount to originating accounts,
  guarded by ALLOW_NEGATIVE_BALANCE; sets cancelled_at+reason).
- No other transitions. Paid is derived, not a status.

## Acceptance criteria
- [ ] AC1: Draft creates lines, no movements, no transactions.
- [ ] AC2: Confirm Cash assigns `YYYY-SALE-SEQ`, deducts stock, creates 1 Income.
- [ ] AC3: Confirm Credit deducts stock, creates zero Income, due = total.
- [ ] AC4: Credit payments create Incomes; overpay => 400; Paid when due = 0.
- [ ] AC5: Confirm with unknown product/account => 404; qty <= 0 => 400.
- [ ] AC6: Double confirm => 400/409; edit Confirmed => 400.
- [ ] AC7: Cancel Confirmed re-enters stock + refunds (Expense), respects negative-balance guard.
- [ ] AC8: sale_number UNIQUE, immutable, NULL only in Draft/Cancelled-from-Draft.
- [ ] AC9: Service lines sellable without stock movement.
- [ ] AC10: finance/stock rows only via services (no direct writes), reference = sale_number.
