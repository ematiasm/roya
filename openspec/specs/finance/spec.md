# Capability: finance (M0)

> Provenance: this capability predates the design conversations that produced the other three.
> Its spec was reconstructed from the implementation and the delivered changes on 2026-09-16.
> Behaviour stated here is verified by the test suite, not by a reviewed upstream proposal.

## Purpose
Hold the money: where it is, how much there is, and which document caused each movement.

## Entities

### accounts
`id`, `name` (UNIQUE, trimmed, non-empty, ≤ 64), `cached_balance`, `created_at`.

### transactions
`id`, `account_id` → accounts ON DELETE CASCADE, `kind` (`Income` | `Expense`), `amount`
(Decimal as TEXT), `description` (≤ 256), `date`, `created_at`, `reference` (nullable).

`reference` holds the source document number, for example `2026-SALE-000012`. Finance stores it as
an opaque string: it knows a movement came from *something* and never what.

### payment_methods
`id`, `name` (UNIQUE), `is_active`. Seeded with `Cash`, `Transfer`, `Debit`, `CreditCard`, `QR`.
There is deliberately no catch-all `Other`, so every payment names an explicit medium.

### account_payment_methods
`(account_id, method_id)` primary key, both ON DELETE RESTRICT. The allowlist that decides which
mediums an account may accept, so cash cannot be booked into a bank account.

## Rules
- `amount > 0`; the account must exist; `description` ≤ 256 characters.
- **Derived balance:** `SUM(Income) − SUM(Expense)` per account, summed in Rust. `cached_balance` is
  synchronised but never read as truth.
- **Negative guard:** with `ALLOW_NEGATIVE_BALANCE=false` (the default), an `Expense` that would push
  the balance below zero is rejected with 400. This applies to creation, to edits, and to deleting an
  `Income`. With the flag set to true, overdraft is permitted.
- A payment pair `(account_id, method_id)` outside the allowlist is rejected with 400 and produces no
  side effect. Account creation requires at least one allowed method; accounts without configured
  methods are visibly flagged in the interface.
- `PUT /api/accounts/{id}/payment-methods` **replaces** the set rather than merging it. Unknown method
  ids return 404 and an empty list returns 400; both leave the existing set untouched.
- Deleting a transaction that a payment references is refused with 409 and an actionable message: the
  money entry belongs to a document, so the document must be cancelled instead.

## Interface
- REST: `GET/POST /api/accounts`, `GET /api/accounts/{id}`, `GET/POST /api/transactions`,
  `PUT/DELETE /api/transactions/{id}`, `GET/PUT /api/accounts/{id}/payment-methods`.
- Web: `/` dashboard with total and per-account balances, `/accounts/{id}` detail with the
  payment-method matrix.
- Configuration: `ALLOW_NEGATIVE_BALANCE` (default `false`), `DATABASE_URL`, `PORT`, `RUST_LOG`.

## Verification
`src/services/transaction.rs`, `src/services/finance_methods.rs`, `src/routes/api.rs` and the
money invariants in `src/smoke_tests.rs`, which assert that each account's reported balance equals
the sum of its transactions and that every payment links to a real transaction.
