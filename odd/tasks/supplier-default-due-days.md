# Feature: Supplier default due days

## Status

**Deferred by the user (2026-09-23)** — recorded so it is not lost, not started.

## Request

Add a field to the `suppliers` table holding each supplier's default payment
term, so a purchase does not make the operator retype it every time. The user's
words: *"agregar un campo a la tabla de suppliers: due_date para dejar un default
de los dias de vencimientos para cada supplier y que el header de purchase cargue
automatico."*

## The conflict to settle before any code

The request as written cannot be implemented literally, and the next session must
resolve this with the user first.

`purchase-payment-at-confirm` deliberately moved the due date **out of the
purchase header**. The recorded decision: the draft asks only WHO and WHEN, the
payment type and the due date are decided in the **confirm dialog**, and the
header edit route passes `due_date: None` precisely so a header edit can never
clear a stored due. The purchase header has no due field by design.

So "the header of purchase loads it automatically" needs a decision:

1. **Pre-fill the confirm dialog's due-date input** (recommended). A Credit
   purchase computes `purchase_date + supplier.due_days` into the field the
   operator already reviews and can override. This delivers the ask and respects
   the recorded decision.
2. Add a due field to the header, which would **reverse**
   `purchase-payment-at-confirm` and needs that spec amended rather than
   contradicted.

## Design questions

1. **Store days, not a date.** A term is `due_days INTEGER` (nullable) on
   `suppliers`; an absolute date would be a one-off, not a default. The due date
   is then derived from the purchase date at confirm time.
2. **Credit only.** `validate_dates` rejects a due date on a Cash purchase, so
   the default applies only when the payment type is Credit. A Cash purchase must
   neither be given a due nor be blocked by one.
3. **A manual value wins.** If the operator types a due date, the supplier's
   default must not overwrite it on a later render.
4. **Never a silent money-driving default.** This codebase has been burned twice
   by a defaulted value that silently drove money — the purchase line's cost
   resolving to the wrong supplier's price, and the `payable 0` that made a
   settled purchase read as red. The defaulted due must be **visible in the field
   it fills**, never applied invisibly at confirm.
5. **It drives derived state, so it needs tests.** The purchase list's `Overdue`
   chip compares `due_date` to today, and the supplier-payment allocation orders
   oldest debt first by `due_date`. A wrong default changes what reads as overdue
   and which debt is paid first — less dangerous than a price, still worth
   pinning.

## Scope sketch

- Migration adding the nullable column to `suppliers`.
- `Supplier` model, repository read/write, `UpdateSupplier`, and `validate_supplier`
  (a negative term must be refused; zero is a legitimate "due on receipt", so
  decide whether NULL and 0 differ).
- The supplier edit form in the drawer (`templates/partials/supplier_detail.html`)
  gains the field, in the repo's own form conventions.
- The pre-fill at the chosen entry point, plus tests: a manual value wins, a Cash
  purchase is unaffected, and a supplier with no term leaves the field empty.
- README and the relevant spec.

## Why it is deferred

The user asked for it during the `purchases-create-and-header` work and chose to
schedule it separately. It is a schema change with a spec conflict, so it wants
its own feature doc, its own migration, and the decision above settled first.
