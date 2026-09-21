# Capability: sales (M2)

## Purpose
Sell goods and services, whether paid now or on account, and connect each sale to the stock it moved
and the money it produced — without sales ever writing to those tables directly.

## Entities

### doc_sequences
`doc_type`, `year`, `last_number`, primary key `(doc_type, year)`, created lazily on first use.
Sales consume `('SALE', year)`.

### sales
`id`, `sale_number` (UNIQUE, nullable), `status` (`Draft` | `Confirmed` | `Cancelled`),
`payment_type` (`Cash` | `Credit`), `customer_id` NOT NULL → customers ON DELETE RESTRICT,
`customer_name` (a frozen snapshot of the customer's name at creation, ≤ 128), `sale_date`,
`due_date` (nullable, required for credit), `receipt_no` (nullable, UNIQUE), `notes`,
`cancel_reason`, `created_at`, `updated_at`, `confirmed_at`, `cancelled_at`.

A sale carries no account: accounts live on each payment, because a credit sale may be paid into
different accounts over time. The customer, the credit rules and the receivable reads belong to the
`customers` capability.

### sale_lines
`id`, `sale_id` → sales ON DELETE CASCADE, `product_id` → products ON DELETE RESTRICT, `qty` (> 0),
`unit_price` (frozen from the product at confirmation, editable while a draft).

### sale_payments
`id`, `sale_id` → sales ON DELETE CASCADE, `account_id` → accounts ON DELETE RESTRICT,
`method_id` → payment_methods ON DELETE RESTRICT, `amount` (> 0), `date`, `created_at`,
`transaction_id` → transactions ON DELETE RESTRICT, `refund_transaction_id` → transactions ON DELETE RESTRICT,
`receipt_id` nullable → customer_receipts ON DELETE RESTRICT.

## Rules
- **A draft touches nothing:** no stock movement, no finance entry, no document number.
- **Confirmation** assigns `YYYY-SALE-NNNNNN`, freezes line prices, issues one `Out` movement with
  reason `Sale` for each stock-tracked line (services move no stock), and then, for cash, records one
  payment and one `Income`. A credit sale creates a receivable instead: no finance entry until it is paid.
- **Derived, never stored:** `total = SUM(qty × unit_price)`, `paid = SUM(payments)`,
  `due = total − paid`. Paid is a derived state, not a status.
- A sale cannot be confirmed twice, and a confirmed sale cannot be edited. Only a draft can.
- **Cash only at confirmation:** the method must belong to an account (ownership is resolved
  before any document number, stock or finance write, and the account is derived from it).
  An unknown method returns 404, an inactive or unassigned one returns 400 with no side effect.
- **Credit payments:** each payment posts one `Income` and links it through `transaction_id`; the sale
  is paid when `due = 0`. Overpayment returns 400.
- **Cancelling** a confirmed sale returns the goods (`In`, reason `Sale-return`) and refunds the money
  already collected as an `Expense` per payment, back to each payment's originating account. The refund
  is guarded by `ALLOW_NEGATIVE_BALANCE`, because the money leaves the account. The original
  `transaction_id` stays intact while `refund_transaction_id` records the reversal.
  The balance guard is evaluated on the AGGREGATE of refunds per account, before any write: two
  payments of 60 on one account whose balance is 100 refuse the annulment (`100 - 120 < 0`) even
  though each payment alone would pass. Nothing is written when the guard rejects, and if any
  payment already carries a `refund_transaction_id` the annulment was partially applied by an
  earlier attempt: it is refused, not doubled — the residual must be resolved, never a second
  return movement or a duplicate refund written over it.
- Cancelling a draft discards it and leaves `sale_number` null.
- The same product may appear on several lines of one sale, which is legitimate when some units carry
  a different price. A purchase does not allow it, for the reason stated in that capability's spec.

- **Credit sale with no customer term:** a credit sale without a due date defaults to
  `sale_date + payment_days`; with no term and no due date it returns 400. Credit to the walk-in is
  refused. See the `customers` capability for the walk-in, the credit limit and the receivables.
- **A payment may be grouped under a receipt**, which is how one handover of money is recorded across
  several sales. A payment without a receipt is a direct payment on one sale and stays valid. See the
  `customers` capability for the collection flow.

## Interface
- REST: `GET/POST /api/sales`, `GET /api/sales/debt`, `GET/PUT /api/sales/{id}`,
  `POST /api/sales/{id}/lines`, `PUT/DELETE /api/sales/lines/{line_id}`,
  `POST /api/sales/{id}/payments`, `POST /api/sales/{id}/confirm`, `POST /api/sales/{id}/cancel`.
- Web: `/sales` with the sale list, status and payment badges, the debt fragment, the draft line editor,
  and confirm, pay and cancel forms. Those forms post to collection endpoints carrying the sale id in
  the body, because htmx uses the literal `hx-post` value and ignores a form's `action`.
- No configuration of its own; it inherits `ALLOW_NEGATIVE_STOCK` from inventory and
  `ALLOW_NEGATIVE_BALANCE` from finance.

## Authorization
Every sales route requires the permission its action declares, enforced by the security kernel — the
route → permission table in `identity/spec.md` is the authority. The cross-capability pair fails
closed: a payment on a sale is `customers.collect` (money received against an owed balance is a
collection), so a principal holding `sales.create` without `customers.collect` cannot pay the sale
it recorded, and no seeded matrix separates the pair. The sale-debt report is `sales.read` (it is
unpaid SALES, not customer data), and confirming a cash sale stays `sales.create` even though it
embeds the tender. The customer statement, reached from the customers department, is a single
`customers.read` gate — not an AND with `sales.read` — so a `customers.read`-only principal sees
that customer's own sale documents without holding `sales.read`.

## Verification
`src/services/sales.rs` (AC1–AC7 plus triangulation), `src/routes/sales_api.rs`,
`src/routes/sales_web.rs`, and the cash sale, credit sale and cancellation flows in
`src/smoke_tests.rs`.
