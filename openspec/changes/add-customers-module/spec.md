# Spec: add-customers-module

## Entities

### customers
- `id PK`, `name TEXT NOT NULL` (trimmed, non-empty, ≤ 128, **not unique**)
- `phone TEXT NULL` (≤ 32), `address TEXT NULL` (≤ 256), `tax_id TEXT NULL` (≤ 32), `notes TEXT NULL` (≤ 512)
- `is_walkin INTEGER NOT NULL DEFAULT 0`
- `is_active INTEGER NOT NULL DEFAULT 1`
- `credit_limit TEXT NULL` (Decimal ≥ 0; NULL means no limit)
- `payment_days INTEGER NULL` (≥ 0; default term applied to credit sales)
- `created_at`, `updated_at`
- Seed: exactly one row `name = 'Consumidor final'`, `is_walkin = 1`, no limit, no term.
- Rules: the walk-in cannot be deleted or deactivated; a second walk-in cannot be created; deleting a
  customer with sales is refused (RESTRICT).

### sales (modified)
- Adds `customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE RESTRICT`, indexed.
- `customer_name` remains and holds the customer's name as a frozen snapshot at creation time.
- Backfill: existing sales are assigned to the seeded walk-in.
- Rule: a credit sale whose customer is the walk-in is rejected with 400.

### customer_receipts
- `id PK`, `customer_id NOT NULL` → customers RESTRICT
- `account_id NOT NULL` → accounts RESTRICT, `method_id NOT NULL` → payment_methods RESTRICT
- `total TEXT NOT NULL` (Decimal > 0), `date TEXT NOT NULL`, `notes TEXT NULL` (≤ 256), `created_at`
- Indexes on `customer_id` and `date`.

### sale_payments (modified)
- Adds `receipt_id INTEGER NULL REFERENCES customer_receipts(id) ON DELETE RESTRICT`, indexed.
- A payment without a receipt is a direct payment on a single sale and stays valid.

## Derived reads
- `customer_balance = SUM(credit sales total) − SUM(payments received on those sales)`
- `customer_debt_sales`: confirmed credit sales with `due > 0`
- `ageing` per customer from `due_date` against today: `current`, `overdue_1_30`, `overdue_31_60`,
  `overdue_61_plus`
- `over_limit`: `credit_limit` is set and `customer_balance > credit_limit`
- `receipt_total(receipt) = SUM(sale_payments.amount WHERE receipt_id = receipt)`

## Rules
- **Credit and the walk-in:** confirming a credit sale for the walk-in returns 400.
- **Credit limit:** on confirming a credit sale, if the customer has a limit and
  `ENFORCE_CREDIT_LIMIT` is true, then `customer_balance + sale_total > limit` returns 400 with the
  projected balance in the message. With the flag false the sale is confirmed and the customer lists as
  `over_limit`. A null limit is never checked.
- **Payment term:** for a credit sale, when `due_date` is omitted it defaults to
  `sale_date + customer.payment_days`; when the customer has no term the due date is required.
- **Receipts:** creating a receipt validates that the customer exists, that the account and method pair
  is in the finance allowlist, and that `total > 0`. Applying a receipt to a sale validates that the
  sale belongs to the same customer, is confirmed, and that the applied amount does not overpay it.
- **Receipt total invariant:** the sum of the payments carrying a receipt equals the receipt total. The
  database refuses to delete a receipt still referenced by a payment.
- Every receipt allocation still posts its own finance movement linked through
  `sale_payments.transaction_id`, and a receipt produces no movement of its own.
- Duplicate names are allowed. Creating a customer whose name matches an existing one returns the
  matches so the interface can warn, and never blocks.
- Sales read customers exclusively through `CustomerService`; no cross-module SQL.

## Configuration
- `ENFORCE_CREDIT_LIMIT` (default `true`). Documented next to `ALLOW_NEGATIVE_BALANCE` and
  `ALLOW_NEGATIVE_STOCK`; it is not a silent bypass, because a null limit remains unlimited either way.

## Interface
- REST: `GET/POST /api/customers`, `GET/PUT /api/customers/{id}`, `GET /api/customers/{id}/statement`,
  `GET /api/customers/ageing`, `GET/POST /api/customer-receipts`, `GET /api/customer-receipts/{id}`.
- Web: `/customers` list with balance and over-limit badges, `/customers/{id}` statement with the ageing
  breakdown, the sale list and the payment history, and a "cobrar deuda" form that takes an amount and
  applies it oldest-first, creating one payment per covered sale grouped under the new receipt.
- The sale form gains a customer selector defaulting to the walk-in.

## Acceptance criteria
- [ ] AC1: the walk-in is seeded and is the only walk-in; it cannot be deleted or deactivated.
- [ ] AC2: creating a sale without a customer is impossible; a cash sale defaults to the walk-in.
- [ ] AC3: a credit sale for the walk-in returns 400 and touches nothing.
- [ ] AC4: a credit sale within the limit is confirmed; over it returns 400 with the projected balance.
- [ ] AC5: with `ENFORCE_CREDIT_LIMIT=false` the same over-limit sale is confirmed and the customer
      reports `over_limit`.
- [ ] AC6: a null limit never blocks, with the flag on or off.
- [ ] AC7: `due_date` defaults from `payment_days`; without a term and without a due date a credit sale
      returns 400.
- [ ] AC8: `customer_balance` equals credit sales minus payments, and a cancelled sale stops counting.
- [ ] AC9: ageing buckets sum to the customer's total debt.
- [ ] AC10: a receipt groups several payments, its total equals their sum, and each payment keeps its own
      `transaction_id`.
- [ ] AC11: applying a receipt to another customer's sale returns 400 with no side effect; overpaying a
      sale through a receipt returns 400.
- [ ] AC12: a payment without a receipt still works exactly as before.
- [ ] AC13: deleting a receipt referenced by a payment is refused.
- [ ] AC14: deleting a customer with sales is refused; deactivating works and keeps history.
- [ ] AC15: a duplicate customer name is accepted and the existing matches are reported.
- [ ] AC16: existing sales are backfilled to the walk-in and their `customer_name` snapshot is preserved.
- [ ] AC17: no module performs SQL against another module's tables; finance never learns about customers.
