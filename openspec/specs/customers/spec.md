# Capability: customers (M4)

## Purpose
Know who owes money, how much and for how long; stop selling credit past a limit; and record a handover
of money that covers several invoices at once.

## Entities

### customers
`id`, `name` (≤ 128, **not** UNIQUE), `phone`, `address`, `tax_id`, `notes`, `is_walkin`, `is_active`,
`credit_limit` (nullable Decimal; null means no limit), `payment_days` (nullable), `created_at`,
`updated_at`.

One row is seeded: `Consumidor final`, with `is_walkin = 1`. A partial unique index enforces at most one
walk-in.

### sales (added by this capability)
`customer_id` NOT NULL → customers ON DELETE RESTRICT, indexed. `customer_name` remains as a **frozen
snapshot** of the customer's name at creation, so correcting a customer never rewrites history. Sales
created before this capability were backfilled to the walk-in.

### customer_receipts
`id`, `customer_id` → customers RESTRICT, `account_id` → accounts RESTRICT, `method_id` →
payment_methods RESTRICT, `date`, `notes`, `created_at`.

**There is deliberately no `total` column.** The amount handed over is derived as the sum of the payments
the receipt groups. A stored total can contradict its own allocations: verification injected a failure
partway through a collection and left a receipt claiming 80 while it had applied 30, and it could not even
be deleted because payments referenced it. A derived total cannot make that claim, which is the same rule
this project applies to balance, stock and debt.

### sale_payments (added by this capability)
`receipt_id` nullable → customer_receipts ON DELETE RESTRICT, indexed. A payment without a receipt is a
direct payment on a single sale and remains fully valid.

## Rules
- **Credit and the walk-in.** Confirming a credit sale whose customer is the walk-in returns 400. Cash may
  be anonymous, which in this model means booked to the walk-in.
- **The walk-in is permanent.** Exactly one exists, and it can never be deactivated, deleted or demoted to
  a regular customer. Three database triggers enforce this, and the connection enables
  `PRAGMA recursive_triggers` so that the `INSERT OR REPLACE` family cannot delete a protected row without
  firing the delete trigger. A service-level check also exists, but only as a friendlier first responder.
- **Credit limit.** With `ENFORCE_CREDIT_LIMIT` true (the default), confirming a credit sale whose
  projected debt exceeds the customer's limit returns 400 quoting the projected figure. With the flag
  false the sale is confirmed and the customer reports `over_limit`. A null limit is never checked, so the
  flag is never a silent bypass. The check runs before any document number, stock or finance write.
- **Payment term.** A credit sale without a due date defaults to `sale_date + payment_days`; with no term
  and no due date it returns 400.
- **Derived balance:** credit sales total minus the payments received on them. A cancelled sale
  contributes nothing to either side even though its payment rows are kept, and a cash sale never counts.
- **Derived ageing**, from `due_date` against an explicit `as_of`: `current` (not yet due or due today),
  `overdue_1_30`, `overdue_31_60`, `overdue_61_plus`. A sale with no due date counts as current. The four
  buckets sum exactly to the balance.
- **Statement:** the customer's sales as debits and payments as credits, chronological, with a running
  balance that ends at the balance. The tie-break is a strict total order so the running balance does not
  depend on sort stability.
- **Collecting:** the plan fills the customer's oldest debt first, ordered by due date, then sale date,
  then id, with a partial on the last covered sale. It creates one payment per covered sale, grouped by a
  new receipt. An amount above the outstanding debt, or zero or less, returns 400; an inactive or
  unassigned method returns 400 (no account can be derived from it). Both are validated before any write.
- **A payment can never be grouped under another customer's receipt.** Two database triggers compare the
  receipt's customer against the sale's customer on insert and on update of `sale_payments`. No route
  accepts a caller-supplied receipt id, so the mismatch is unreachable from the interface by construction.
- **Deleting a receipt** that a payment references is refused. Deleting a customer with sales is refused;
  deactivate instead. The walk-in can never be deleted or deactivated.
- Duplicate names are allowed, because two people share names. Creating or renaming onto an existing name
  succeeds and returns the matches so the interface can warn.

## Configuration
`ENFORCE_CREDIT_LIMIT` (default `true`), documented next to `ALLOW_NEGATIVE_BALANCE` and
`ALLOW_NEGATIVE_STOCK`.

## Interface
REST: `GET/POST /api/customers`, `GET/PUT/DELETE /api/customers/{id}`, `POST /api/customers/{id}/activate`,
`POST /api/customers/{id}/deactivate`, `GET /api/customers/{id}/statement`, `GET /api/customers/ageing`,
`GET/POST /api/customer-receipts`, `GET /api/customer-receipts/{id}`. There is deliberately no route that
accepts a receipt id from a caller.

Web: a customers page with balance, ageing and over-limit badges plus the create and edit forms, and a
statement page with the ageing breakdown, the receivable sales, the payment history and the collect form.

## Known residuals
- **An orphan finance movement is possible.** A failure between posting the money and inserting the payment
  leaves an Income claimed by no payment. The project deliberately does not share transactions across
  modules, so the residual is not eliminated; it is **detected** by the smoke invariant, which requires
  every transaction with a document-shaped `reference` to be claimed by a payment as its `transaction_id`
  or `refund_transaction_id`.
- **The receipt total is gross, not net.** Cancelling a sale keeps the payment row and sets
  `refund_transaction_id`, so a receipt still reports what it applied at collection time. That is honest as
  "amount applied", not as the customer's net position.
- **Parent-row tampering** breaks the guarantees: `UPDATE sales SET customer_id`, `UPDATE
  customer_receipts SET customer_id`, or `REPLACE INTO sales` cascading its payments. None of these is
  reachable from a route or a repository, only from direct SQL.

## Verification
`src/services/customers.rs`, `src/services/customer_receipts.rs` and the receivable reads in
`src/services/sales.rs`, with route coverage in `src/routes/customers_api.rs` and
`src/routes/customers_web.rs`. The smoke suite adds the pages to the wiring guard and runs the full
collection flow over HTTP, asserting the derived balance, the ageing and the receipt total against its
allocations. Backward compatibility was verified by copying the real database and applying the migration
chain, confirming every pre-existing table unchanged and the walk-in seeded exactly once.
