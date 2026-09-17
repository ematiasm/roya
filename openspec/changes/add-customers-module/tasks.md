# Tasks: add-customers-module

## Review Workload Forecast
- Estimated: ~1500-1700 lines across three slices (4 migrations, models, 2 repositories, 2 services,
  sales wiring, 2 route files, templates, tests, plus updating the sales tests and the smoke suite for
  the mandatory customer).
- Chained PRs recommended: Yes — 3 slices.
- 400-line budget risk: High for a single PR.
- Decision needed before apply: Yes.

## Slice K — customers entity and the sales link
- [ ] T1: migrations `create_customers` (with the guarded walk-in seed) and `add_sales_customer`
      (column, backfill, rebuild enforcing NOT NULL)
- [ ] T2: models `Customer`, `NewCustomer`, `UpdateCustomer`, `CustomerBalance`, `Ageing`
- [ ] T3: `CustomerRepository` trait and SQLite impl, with duplicate-name lookup
- [ ] T4: `CustomerService`: CRUD, deactivate, RESTRICT-aware delete, walk-in protection, duplicate
      warning, derived balance and ageing, `over_limit`
- [ ] T5: sales wiring: mandatory `customer_id`, snapshot of `customer_name`, credit rejects the
      walk-in, credit limit with `ENFORCE_CREDIT_LIMIT`, due date from `payment_days`
- [ ] T6: tests for AC1-AC9 and AC14-AC16, plus updating the existing sales tests and the smoke suite
      for the new mandatory field

## Slice L — customer receipts
- [ ] T7: migrations `create_customer_receipts` and `add_sale_payments_receipt`
- [ ] T8: models `CustomerReceipt`, `NewReceipt`, `ReceiptDetail`
- [ ] T9: `CustomerReceiptRepository` trait and SQLite impl
- [ ] T10: collection service: apply an amount oldest-first, create one receipt grouping one payment
      per covered sale, validate same-customer and no overpay
- [ ] T11: tests for AC10-AC13, including the receipt total invariant and traceability preservation

## Slice M — routes and UI
- [ ] T12: REST for customers, statement, ageing and receipts; wire `CustomerService` into `AppState`
      and add `ENFORCE_CREDIT_LIMIT` to the environment
- [ ] T13: Web: the customers list page and the customer statement with the ageing breakdown, plus the
      collect form and the customer selector on the sale form
- [ ] T14: route-level tests, and the smoke suite extended with the collect flow
- [ ] T15: README and `env.example` updated

## Verify
- [ ] `cargo test` green (169 baseline plus the new tests), the wiring guard still passing on the new
      pages, and a manual smoke: create a customer, sell on credit, collect part of it, check the
      statement and the ageing, then confirm the receipt total equals the sum of its payments.

## Archiving
- [ ] On merge: promote the customer requirements into `openspec/specs/customers/spec.md`, update the
      `sales` capability spec for the mandatory customer and receipts, and move this change to
      `openspec/changes/archive/`.
