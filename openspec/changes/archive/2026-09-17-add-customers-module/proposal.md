# Proposal: add-customers-module (M4 Clientes)

## Workflow
ODD with OpenSpec artifacts (no SDD phase agents on this machine). Slices of the order of one to two
hundred lines each, independent verification per slice, archive on merge.

## Problem statement
Credit sales are already supported, but the customer behind them is free text
(`sales.customer_name`, default empty). Nothing in the system can answer "how much does this person
owe me". There is no customer record, no per-customer balance, no credit limit, no payment term and no
idea of how old a debt is. A polirrubro that sells on account needs exactly those four things, and it
needs to stop someone from being sold more credit when they already owe a lot.

Two secondary problems follow from the same gap. A customer who hands over a lump sum covering several
invoices cannot be served: today every payment belongs to exactly one sale, so the shop has to split
the money by hand, or invent it. And the existing `sales.customer_name` snapshot will drift away from
any customer record once one exists.

## Goal
Introduce a real customer entity and wire it into sales, with a receivable that is derived rather than
stored, and add a receipt document that groups the payments produced by one handover of money.

## Scope frozen with the user

### customers
`id`, `name` (≤ 128, **not** UNIQUE), `phone`, `address`, `tax_id`, `notes`, `is_walkin`, `is_active`,
`credit_limit` (nullable: null means no limit), `payment_days` (nullable, default term for credit),
`created_at`, `updated_at`.

**A walk-in customer is seeded** (`is_walkin = 1`, "Consumidor final") and `sales.customer_id` becomes
`NOT NULL`. This is better than a nullable column: every sale has an owner, the model has no null
cases, and "Consumidor Final" is a concept real invoicing will require anyway.

### sales
`customer_id` NOT NULL → customers ON DELETE RESTRICT. `customer_name` stays as the frozen snapshot of
the name at the time of the sale, so correcting a customer later never rewrites history. Existing sales
are backfilled to the walk-in.

### customer_receipts
`id`, `customer_id` → customers RESTRICT, `account_id` → accounts RESTRICT, `method_id` →
payment_methods RESTRICT, `total`, `date`, `notes`, `created_at`.

`sale_payments` gains `receipt_id` (nullable) → customer_receipts RESTRICT.

This is the hybrid the user chose: keep the simple model where each payment belongs to a sale, and
add a grouping document recording what was actually handed over and how it was applied. Each payment
still posts its own movement, so the traceability built in the previous change is untouched, and the
invariant `receipt.total = SUM(allocations)` is checkable. If the accounting is ever moved to one
movement per receipt, the allocation data is already in the right shape.

## Rules
1. **Credit rejects the walk-in customer.** You cannot sell on account to "Consumidor final". Cash may
   be anonymous, which in this model means booked to the walk-in.
2. **Credit limit** is enforced when set and `ENFORCE_CREDIT_LIMIT` is true (the default): confirming a
   credit sale whose projected debt exceeds the limit returns 400. With the flag false the sale is
   allowed and the customer is flagged as over limit in the interface. A null limit means no limit, so
   the flag never acts as a silent bypass.
3. **Payment term:** for a credit sale the due date defaults to `sale_date + customer.payment_days` and
   can still be overridden.
4. **Derived customer balance:** credit sales total minus the payments received on them. Never stored.
5. **Derived ageing** from `due_date`: current, 1–30, 31–60, over 60 days overdue.
6. **A receipt can only be applied to sales of its own customer**, and its total must equal the sum of
   the payments it groups.
7. A name is not unique, but creating a customer whose name already exists returns the existing matches
   as a warning rather than blocking, because two people can share a name.
8. Deleting a customer with sales is refused (RESTRICT); deactivate instead. The walk-in cannot be
   deleted or deactivated.
9. Sales reach customers only through `CustomerService`. No cross-module SQL, and finance stays ignorant
   of customers exactly as it is of sales.

## Out of scope for v1
Opening balances and manual adjustments (the user confirmed no existing debt needs migrating), printed
statements, overdue interest, payment reminders, multiple contacts per customer, and migrating to one
accounting movement per receipt.

## Known impact
`customer_id` becomes mandatory on sale creation, so the sales API, the sales and purchase web forms,
the route tests and the smoke suite all need updating. This is a deliberate, contained breaking change
on a system with a single user and one sale in the database.

## Acceptance summary
Walk-in seeded and undeletable; credit to walk-in rejected; limit enforced and flaggable; due date from
the payment term; customer balance and ageing derived; receipt groups payments with a checkable total
and a same-customer rule; every payment keeps its transaction link; no cross-module SQL.
