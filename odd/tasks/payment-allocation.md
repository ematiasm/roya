# Payment allocation — one delivery of money, many documents

## Status

**In progress — P1, P3 (a/b/c/d) and P4 are DONE; P5, P6, P7 and P8 remain.**
**P5 was re-scoped on 2026-10-09 and then SIMPLIFIED by two user decisions, before any
code was written.** The re-scope (P5a journal + P5b reads) existed because the ledger
did not close `balance == Σ residuals + unapplied`. Two decisions removed that reason:
the legacy dual-write is **deleted** rather than retired (a), and `cancel` **stops
existing** as a business action (b), so there is no annulment left to journal. P5 is one
task again — the reads, the legacy deletion and the `In`-only unapplied fold together —
because the first fold to break when the legacy rows go is the one being replaced. See
"P5 blocker" for the evidence and the decisions, and "Relationship" for the wall a
credit note still hits.
Branch `feat/party-ledger`, all pushed, at `733acc9`. **Tracker PR open as DRAFT, no merge:
[#150](https://github.com/ematiasm/roya/pull/150)**, on issue
[#149](https://github.com/ematiasm/roya/issues/149). Do NOT merge before P8: merging with
the legacy tables still in place leaves two homes for the same truth, which is the disease
this work exists to cure.

**Decision (user, 2026-10-09): ONE PR for the rest, not a stacked chain.** The
dependency P6→P5 is real, but the size is not a reason to split: the legacy dual-write
deletion (decision a) forces P5 and part of P8 together anyway, so the remaining slices
ship as one PR on `feat/party-ledger`, appended to the tracker's branch and to PR
[#150](https://github.com/ematiasm/roya/pull/150). The earlier `feat/party-ledger-p5` /
`-p6` branch plan is dropped.
Last green measurement: `cargo test --locked` **1554 passed / 0 failed**,
`scripts/e2e.sh` **180 passed / 0 failed**, warnings 79 bin / 55 test.

What that means in the tree, so the next session can verify it rather than trust it:
migration 46 (`payments` + `payment_allocations`) and 47 (`payments.receipt_id`) exist;
`services::payment_writer::record_delivery_in` is the ONE writer of a delivery of money
and four services call it; the four `confirm` paths write ledger entries; all four
`cancel` paths run in one unit; a collection and a supplier payment are each ONE
document with N allocations.

**A previous version of this header said "Planned, no code written yet"** — it was 27
commits stale. If you are reading a Status line that contradicts `git log`, the log wins.

This document **reshapes** tasks T3a, T3b, T4 and T5 of `odd/tasks/party-ledger.md` —
see "Relationship to the party ledger". T1 and T1b of that document are done; T2 is
half done (its four entry writes are in, its two refund caps are still open by
decision).

## Objective

One document per **delivery of money** — a customer handing over cash, a transfer
covering several invoices, a cheque, a refund — with its own number, its own single
cash movement, and an explicit list of which documents that money pays. One shared
family for customers and suppliers, because a supplier payment covers one or
several invoices just like a collection does.

Today no such object exists. That absence is the root of four separate business
flows being impossible or wrong.

## Problem — in business flows, then in code

**1. Collecting several invoices at once is guesswork.** The operator types an
amount and the system distributes it oldest-due-first (`plan_allocations`,
`services/customer_receipts.rs:238-259`); the operator never chooses which
invoices, and no route accepts invoice ids
(`templates/partials/customer_detail.html:42-62` posts only `amount`). One bill
covering three invoices writes **three** cash movements
(`customer_receipts.rs:152-166` loops `record_payment_with_receipt`, one `Income`
per sale at `services/sales.rs:1564-1576`), and the grouping document —
`customer_receipts`, whose total is derived (`models.rs:2892`) — **has no number**
(`migrations/20240101000032_add_audit_sales_customers.sql:191-201`), so there is no
single numbered receipt to hand the customer or to cite later. The only number a
customer sees is `sales.receipt_no`, an operator-typed field on the *sale*.

**2. Money on account is impossible.** `sale_payments.sale_id` is `NOT NULL`
(`…32:145-160`), so every payment row must name exactly one sale, and four sites
refuse paying more than the outstanding balance (`sales.rs:1553`,
`purchases.rs:1296`, `customer_receipts.rs:110`, `purchases.rs:1378`). A customer
leaving 10.000 "a cuenta", or paying 250 against 200, has nowhere to exist. That is
the saldo a favor the party ledger can only represent as a nameless negative
number.

**3. An allocation cannot be corrected, because it does not exist as an object.**
The attribution lives inside the same row as the cash movement, so the only
"correction" is deleting and re-registering, which moves money.

**4. Paying a supplier for several invoices has no document at all.**
`purchase_payments` has no `receipt_id` (`…33:218-232`) and `pay_supplier` loops
`record_payment` (`services/purchases.rs:1301-1419`): N expenses, N rows, nothing
that says "on Tuesday I handed this supplier 200.000".

**5. Collection is not atomic.** `record_payment_with_receipt` posts the `Income`
in one unit (`sales.rs:1564-1576`) and the payment row in a second
(`:1577-1586`); `collect` and `pay_supplier` loop with no enclosing `begin`. A
failure between the two leaves cash in the box with no document behind it. Live
defect, not hypothetical (party-ledger decision 8).

## Why one shared family and not a promoted receipt

Promoting `customer_receipts` to carry an amount would fix flows 1 and 2 for
customers only, leave the receipt without a number, and leave the supplier side
with no document at all — one concept ("a delivery of money") implemented twice,
diverging, with only half of it numbered. AGENTS.md already states the house rule
that document families mirror each other; this would be the first one deliberately
not to. The user's business decision (2026-10-03): **a supplier payment may cover
one or several invoices**, so the supplier side needs the same object, not a
loop.

## Decisions

1. **Decided (user, 2026-10-03) — one `payments` family for both parties, with a
   direction.** `direction` `'In'` (money received: customer collection) or
   `'Out'` (money handed over: supplier payment, refund to a customer). Fields:
   `party_type`/`party_id`, `direction`, `number` (unique, from `doc_sequences`),
   `method_id`, `account_id`, `amount`, `date`, `notes`, `transaction_id`, audit
   columns. The number comes from the existing no-gap sequence mechanism
   (`migrations/20240101000007_create_doc_sequences.sql:3-8`,
   `repositories/doc_sequence_repo.rs`), the same one that issues `sale_number`
   and `credit_note_number` — so a delivery of money becomes a citable document
   for the first time.
2. **Decided (adopted, 2026-10-03) — the shape first, then the atomicity.** This
   unit builds the document and the allocations **before** rescuing the atomicity
   of the collection paths (party-ledger T3a), so that layer is written once in its
   final shape: the cash movement moves from per-sale to per-delivery. Trade
   accepted explicitly: the live atomicity defect above stays until P3 of this
   unit, because form-first avoids writing the collection layer twice.
3. **Decided (adopted, 2026-10-03) — allocations are mutable state, not a
   journal.** `payment_allocations`, one row per (payment × target document),
   positive amount, `UNIQUE(payment_id, target_kind, target_id)`,
   `updated_by`/`updated_at`. Reallocating is an `UPDATE` inside the owning unit.
   It writes **no** ledger entry and moves **no** cash. Reason: an allocation is
   not folded into any balance — it is subtracted from residuals that are always
   recomputed, and its total is bounded by the payment document. The party
   ledger's append-only discipline exists because the balance **is** the fold of
   its rows; that argument does not transfer here, so mutability puts no total at
   risk while buying the operator the ability to correct an attribution.
4. **Decided (adopted, 2026-10-03) — the schema caps the split.**
   `BEFORE INSERT`/`BEFORE UPDATE` guard on `payment_allocations`:
   `Σ allocations of a payment ≤ that payment's amount`. This is what makes
   `unapplied = delivered − allocated` always ≥ 0 and therefore trustworthy — the
   number that is the saldo a favor (flow 2). It is also what retires pure FIFO:
   the operator chooses the split and the database guarantees he cannot
   over-allocate.

   **REFINED 2026-10-08, with the two limits MEASURED before writing P1.** The
   claim above is true and was tested on a real SQLite 3.53.4, but a trigger on
   one table cannot be the whole of an invariant that spans two, and the split
   needed to be said out loud before the migration was written:

   **What was measured as working.** A `BEFORE INSERT` trigger DOES see the rows
   the same transaction already inserted — 60 + 30 against a payment of 100
   passes, and a third allocation of 20 aborts. That is load-bearing: without it
   this whole shape collapses, because a unit that writes its allocations one at a
   time would only ever see its own row. The `BEFORE UPDATE` twin works too, with
   the one clause that is easy to forget: `AND id <> NEW.id`, or an allocation
   "corrected" to its own current value refuses itself.

   **Limit 1 — lowering the payment's amount slips past the cap SILENTLY.**
   Measured: 70 allocated against a payment of 100, then
   `UPDATE payments SET amount = '10'`, and the result is 70 allocated against a
   payment of 10 with NO error anywhere. The cap looks only at the allocation side;
   the trigger cannot see the parent's `UPDATE`, which lives on another table.
   Since `unapplied = delivered − allocated ≥ 0` is the number the business reads
   as the credit balance, a negative residual here is not cosmetic. **Resolution
   (2026-10-08): a payment's `amount` is IMMUTABLE once it has allocations.**
   Changing what was delivered is a re-issue, not an edit, so the hole closes by
   construction rather than by a check somebody must remember. This is a decision
   about the document, not about the guard.

   **Limit 2 — the trigger needs `CAST(… AS REAL)`, and this project forbids
   `REAL` for money.** SQLite cannot sum `TEXT` decimals, so the guard's arithmetic
   is not the arithmetic of the read: measured, `0.10 − 0.09` in `REAL` is
   `0.010000000000000009`. With round cents the verdict agrees; at an exact
   boundary it is a different sum than the one the code folds, which is where a
   false allow or a false refusal lives. **Resolution (2026-10-08): the cap has
   TWO homes with different roles.**
   * The **service** validation is the real gate: it reads the payment's
     allocations with `_in` inside the caller's unit, folds them in `Decimal` with
     the existing `checked_money_sum`, and refuses with `AppError::Validation` —
     a 400 naming the fix, and it sees the WHOLE pair (payment + allocations)
     because it is not confined to one table. This is where the operator's
     refusal comes from.
   * The **schema** trigger stays as the backstop against direct SQL, in the same
     spirit as migration 44: **INSERT-only**, and accepting that its arithmetic is
     approximate and that it does not see the parent's update. It is a net under a
     hand-written statement, not the rule.

   This refines rather than replaces the original decision: the schema still
   guarantees "you cannot allocate more than was delivered", and the service
   guarantees the stronger claim the trigger could not express. The precedent is
   migration 44, where the same reasoning produced an INSERT-only guard plus a
   named residual hole instead of a cleverer trigger.
5. **Decided (adopted, 2026-10-03) — one delivery, one cash movement.**
   `payments.transaction_id` holds the single `transactions` row, written once,
   with `reference` = the payment number. Allocations carry **no** account, no
   method and no transaction. This is what makes an applied credit representable
   without inventing a fake movement: the money moved when it arrived; applying it
   later is only an allocation.
6. **Decided (adopted, 2026-10-03) — the party ledger records one `Payment` entry
   per payment document**, not per invoice. The balance stays one fold over
   `party_ledger_entries`; the per-document residual comes from the allocations.
7. **Decided (adopted, 2026-10-03) — `payments` inherits migration 44's
   method→account guard** (equality at insert, divergence allowed afterwards),
   because it carries the same pair for the same reason. Requirement recorded in
   `odd/tasks/payment-method-single-account.md`.
8. **Decided (adopted, 2026-10-03; REVISED the same day) — one single home for
   "money applied to a document", and the legacy tables retire instead of being
   migrated.** `payment_allocations` is the only write target from P3/P4 onward.
   **The user confirmed the development database will be wiped — there is no
   production history to preserve — so the 1:1 backfill this decision originally
   planned is cancelled (P2).** The legacy tables (`sale_payments`,
   `purchase_payments`, `customer_return_payments`, `purchase_return_payments`,
   `customer_receipts`) keep their schema — a shipped migration is never edited —
   stop being written in P3/P4, and are **dropped in one cleanup migration once
   nothing references them** (P8), which is also when `backfill_party_ledger` is
   re-based off them.

   Why a single home matters at all: the five legacy tables store the attribution
   **inside the same row as the cash movement**, so leaving them written would put
   the same fact in two places — ageing would read allocations for new payments and
   legacy rows for old ones, and the two would drift. That is the disease this
   whole family of work exists to cure (five folds computing one concept).

   **What the wipe removes, and it is more than a backfill:** the original plan had
   to accept that "one delivery = one cash movement" would **not** be retroactive,
   because committed `Income` rows per invoice are history and consolidating them is
   a bigger sin than an ugly history. With no history, that concession disappears:
   the invariant holds from the first payment, and there is no "except historical"
   footnote in the docs, the tests, or the operator's head.
9. **Decided (adopted, 2026-10-03) — refunds are `direction='Out'` payments.**
   A customer return handing cash back, and a supplier refund, become out-payments
   with their own account and one `Expense`, replacing
   `customer_return_payments` / `purchase_return_payments` as the write target.
   The refund still takes its account from the **parent payment's** stored account
   (`RefundPlan`, `services/customer_return.rs:707-708`,
   `services/purchase_return.rs:723-724`) so the money goes back out of the box it
   came into — which is exactly why the two legacy refund tables are exempt from
   the migration-44 guard. That exemption requirement travels with this unit: the
   refund out-payments must keep replaying the historical pair even when the
   method has since been re-pointed.

## Scope

In scope:

- Migration 46: `payments` + `payment_allocations` + indexes + the inherited
  method→account guard + the split cap guard.
- `PaymentRepository` / allocation access with `_in` twins, trait + `Sqlite…` impl
  in one file, the house shape.
- The one-shot Rust backfill of legacy payments — **CANCELLED (P2)**: the user
  confirmed the development database will be wiped, so there is no history to
  preserve. What replaces it is the retirement of the legacy tables (P8).
- Customer and supplier money paths writing the new shape inside **one** unit:
  `record_payment` (sales/purchases), `customer_receipts::collect`, `pay_supplier`,
  and the refund/out paths of the four cancel and return services. One
  `transactions` row and one `Payment` ledger entry per delivery.
- Explicit allocation (the operator picks the documents and the amounts);
  oldest-first survives only as a *suggestion* the operator can accept.
- Lifting the four overpayment refusals for the party side, with the credit
  becoming the payment's unapplied remainder.
- Reads that need the per-document residual: ageing, credit limit, statement,
  supplier drawer, payables.
- UI: collect/pay screen with document selection, explicit apply of available
  credit, saldo a favor display. `MessageKey` variants + ES + EN; Playwright.

Out of scope (follow-ups, recorded so nobody invents them later):

- Withdrawing an available credit as cash to a customer who asks for it back
  (party-ledger keeps it out of v1 too).
- Crossing parties in one payment: one payment has one party.
- Split tender (300 cash + 200 transfer for the same documents) is **two
  payments**, not one document with two accounts. A later `batch_id` could group
  them for the operator's convenience; not here.
- Consolidating or migrating historical payments (decision 8): the database is
  wiped instead, so there is nothing to carry over. Anything that still references
  the legacy tables is retired in P8, not converted.
- Reopening the blocked party-ledger review lineage or the retired accumulation
  review; delivery decisions stay the user's.

## Constraints

- Binary-only crate: inline `#[cfg(test)] mod tests`, no `tests/` directory.
- Never edit a shipped migration; `sqlx::migrate!` verifies applied checksums.
  Migration 42 must stay byte-identical; 43 and 44 are committed and equally
  frozen.
- Money is `Decimal` over `TEXT`; every fold goes through `checked_money_add` /
  `checked_money_sum` / `checked_aggregate_sum`. A bare `+` on money is a defect.
- `sqlx::migrate!` does not re-read its directory when a file is added:
  `touch src/main.rs` (mtime only) is required after adding a migration.
- `ORDER BY id` is load-bearing where a fold is checked on a running sum.
- New UI string = `MessageKey` variant + both ES and EN catalogs.
- JSON API handlers return named types, never `Json<Value>`.
- Measurement baseline on this tree: 79 bin / 49 test warnings. Compare deltas.
- Never run bare `cargo fmt`.

## Tasks

> **Migration numbers renumbered 2026-10-08.** This plan reserved 45 for
> `payments`; T6 of `odd/tasks/payment-method-single-account.md` landed on 45
> first (the `account_id NOT NULL` rebuild and the `Caja`+`Cash` seed), and that
> unit must come FIRST anyway — a fresh install has to be able to collect before
> anything is built on top of it. So the payment schema moves to **46**, and both
> plans stay monotonic. Everything else in this document is unchanged.

- [x] **P1 — Migration 46 + repository + guards.** `payments`,
  `payment_allocations`, indexes, the method→account guard inherited from
  migration 44, and the split cap (`Σ allocations ≤ payment amount`). Trait +
  `Sqlite…` impl with `_in` twins and a checked residual fold. This layer starts
  unwired on purpose: the warning count goes **up** until P3/P4 call it, exactly
  the shape AGENTS.md predicts.
  Carries the two refinements decision 4 gained on 2026-10-08: the cap lives in
  the SERVICE as the real gate (Decimal, sees the whole pair, 400) **and** in the
  schema as an INSERT-only backstop against direct SQL; and a payment's `amount`
  is IMMUTABLE once it has allocations, which closes the silent hole where
  lowering the delivered figure left the allocations above it. Tests for P1
  therefore include the three the measurement produced: the same-transaction
  insert sees its siblings, the parent's amount cannot be lowered under its
  allocations, and the residual fold refuses with `AggregateTooLarge` rather than
  panicking.
  **CLOSED 2026-10-08 — committed as `4d7917c`** (migration 46, `Payment`/
  `NewPayment`/`PaymentAllocation`/`PaymentDirection`/`format_payment_number`,
  `PaymentRepository` + `SqlitePaymentRepository` with `_in` twins). Ten tests in
  the module; RED by mutation (dropping the existing shares fails the two cap
  tests, `>` to `>=` fails the exact-boundary one). **The layer is UNWIRED on
  purpose, so the warning count went UP: 78 bin / 49 test → 97 bin / 53 test**,
  14 of the new ones this repository's own dead code. That is the shape AGENTS.md
  predicts; the count coming back down in P3/P4 is what proves the wiring is real.
  One thing the tests caught in the writing, recorded because it is the kind of
  mistake that looks like a passing suite: the first draft of `insert_payment` had
  eleven columns and ten binds (it omitted `number`), so every value shifted one
  position and the ACCOUNT received the amount. `NewPayment` now carries `number`
  explicitly, taken by the caller from `doc_sequences` so a rollback returns it
  instead of burning it.
  Not done in P1, and named so it is not read as forgotten: `updated_at` has no
  trigger. The only idiom migration 36 has for one is the `SELECT NEW.x = …`
  comparison that writes nothing (see T6 of the payment-method document), so these
  tables use the plain `updated_at = strftime(…)` the other repositories write.
- [ ] **P2 — CANCELLED (2026-10-03).** The 1:1 backfill of legacy payments is not
  needed: the user confirmed the development database will be wiped and holds no
  production history. Its replacement is P8.
- [ ] **P3 — Customer money paths write the new shape, in one unit.** `collect`,
  **P3a, P3b and P3c are IN; P3d was split out as its own task (below) after being
  measured.** The two
  halves that landed are the ones about the SHAPE of a delivery of money; the two
  that remain are the two customer overpayment REFUSALS being lifted and the
  cancel/refund paths writing `direction='Out'`.
  * P3a — `d94775a`: `record_payment` writes the document, the movement, the share
    and the ledger entry in ONE unit. This closed flow 5 of this plan, a live
    defect: the `Income` used to commit in its own transaction before the row that
    claimed it, and the module had a fixture that injected that exact failure and
    ASSERTED THE ORPHAN as expected behaviour.
  * P3b — `bef1241`: `collect` writes ONE delivery for the whole collection, and
    `record_delivery_in` became the single writer the three `In` entry points share.
    The traceability invariant was RE-BASED with it: it claimed "a transaction
    belongs to exactly one payment", which under decision 5 would now call a correct
    three-invoice collection a violation. The property that holds is about the
    DELIVERY — no transaction is shared ACROSS deliveries — and a new test with two
    mutations proves the difference is measurable. The receipt now lives inside the
    delivery's unit, so `injected_failure_mid_collection_...` asserts six empty
    tables instead of a "coherent" partial receipt.
  * P3c — `5fafd3e`: the two customer refusals are LIFTED and the cap moved one level
    down to the SHARE, plus migration 47 (`payments.receipt_id`) so a receipt can state
    what it took in rather than only what it applied. `ReceiptDetail` gained `applied`
    and `unapplied` beside a `total` that now means received. Lifting the refusals
    exposed a rule that was enforced BY ACCIDENT — a collection against the walk-in
    failed only because its balance is zero — which is now an explicit refusal.
  * Together: `cargo test --locked` 1547 passed, `scripts/e2e.sh` 180 passed,
    warnings 80 bin / 53 test (a P1 peak of 97 → 80 as the layer got called, which is
    the evidence AGENTS.md asks for that the wiring is real).

  The original text follows, unchanged, as the remaining contract. `collect`,
  `record_payment`, and the customer cancel/refund paths: one `payments` row, N
  allocations, one `transactions` row, one `Payment` ledger entry, all inside the
  caller's unit; explicit allocation input (`[(sale_id, amount)]`) with the
  oldest-first plan offered as a default; the two customer overpayment refusals
  lifted. Tests first (RED): a failure injected between the cash row and the
  payment row leaves neither; collecting 3 invoices with one amount writes **one**
  `Income`; overpaying leaves `unapplied > 0` and no error; applying that credit to
  a later sale writes an allocation and **no** cash movement.
- [x] **P3d — The four `cancel` paths write their refund as a delivery, inside one
  unit.**
  **CLOSED 2026-10-08 — committed as `226ef09` (sale), `38bb8cd` (the other three plus
  the shared writer) and `54dae4a` (their pins).** What it turned out to need, beyond
  what this task predicted:
  * the writer had to MOVE. It was a method on `SalesService`, so the other three
    families could not reach it. It is now `services::payment_writer::record_delivery_in`,
    a free function over exactly the four repositories it touches (sequences,
    transactions, ledger, payments) — the callers share no trait, and naming the
    collaborators is what keeps it unable to touch anything else. The move DROPPED a
    warning instead of adding one.
  * six more `_in` twins, not two: `set_payment_refund_transaction_in` and
    `set_cancelled_in` on each of the four repositories, one copy of each statement
    with the public form as the thin wrapper.
  * the direction is per family and follows the money: a sale refund is `Out`, a
    purchase refund `In`, a credit-note reversal `In`, a purchase-return reversal
    `Out` — the one direction where a reversal can be refused for want of funds.
  Observed: `cargo test --locked` 1552 passed / 0 failed; `scripts/e2e.sh` 180 passed /
  0 failed; warnings 79 bin / 55 test, one below where the unit started. Every one of the
  four is pinned by an injected failure on the cancellation UPDATE — the LAST write of
  the unit — and all four were MUTATED rather than assumed: putting the refund back in
  its own transaction makes each fail after 30s with `pool timed out`.
  Resolved along the way, and worth knowing: a cancelled sale ends up with TWO
  deliveries (the `In` and the `Out`), which is consistent with the append-only ledger
  and is the answer to the question this task left open. The refund **replays the parent
  payment's account** (decision 9), reading the historical `account_id`/`method_id` off
  the payment row and never re-deriving them from the method.

- [ ] **P3d — The four `cancel` paths write their refund as a delivery, inside one
  unit.** Split out of P3 on 2026-10-08 after measuring it, because it is NOT the
  small edit the P3 line implied. Measured on the tree:
  **all four cancellations run with no transaction at all**, and each writes its
  refund with `create_with_reference` (a unit of its OWN) and then `set_cancelled`:
  `sales::cancel` (`sales.rs:1922`, refund at `:2064`), `purchases::cancel`
  (`purchases.rs:1490`), `customer_return::cancel` (`customer_return.rs:783`,
  refund at `:880`) and `purchase_return::cancel` (`purchase_return.rs:798`).
  That is flow 5's defect again, with money LEAVING: a failure between the `Expense`
  and `set_cancelled` leaves the money refunded and the document still Confirmed.
  (Contrast `customer_return::confirm`, which DOES have a unit — `:607` — so the
  problem is specifically the four `cancel` paths.)
  The shape to build: each refund becomes a `payments` row with `direction='Out'`,
  the single `Expense`, and the `Refund` ledger entry (decision 9), all inside one
  unit per cancellation. Two consequences to decide at that point, recorded here so
  they are not discovered as surprises:
  * **A cancelled sale then has TWO deliveries** (the `In` and the `Out`), not one
    annulled entry — consistent with the append-only ledger, but it changes what the
    UI shows as "that sale's payment".
  * The refund **replays the parent payment's account** (`RefundPlan`), which is
    exactly why the two refund tables are exempt from migration 44's guard; that
    requirement travels with this unit.
  Forecast: four services, one unit each, plus the `Out` document shape and the
  legacy `*_return_payments` rows until P8. Comparable to P3a in size, not to a
  one-line change.

- [x] **P4 — Supplier money paths, mirrored.** `pay_supplier`, purchase
  `record_payment`, purchase cancel/refund: one `payments` (`direction='Out'`) with
  N allocations. Same tests mirrored, plus: one handover of 200.000 covering four
  bills is one document and one `Expense`.
  **CLOSED 2026-10-08 — committed as `1a1b867`.** All three parts, and the third was
  already done: **the purchase cancel/refund was covered by T3d**, which put all four
  `cancel` paths in one unit and made the refund a delivery (`direction='In'` on this
  side, because the money comes back to the shop).
  * `record_payment` is one unit now — it had the purchase twin of the defect P3a
    closed: the `Expense` in one transaction and the row in a second.
  * `pay_supplier` is ONE delivery instead of a loop over `record_payment` with no
    enclosing `begin`, which produced four `Expense`s for one handover of 200.000 and
    nothing tying them together. One document, one movement, one share per covered
    invoice, one `Payment` ledger entry.
  * the plan's named case has a test with its name:
    `one_handover_over_four_bills_is_one_document_and_one_expense`.
  Observed: `cargo test --locked` 1554 passed / 0 failed; `scripts/e2e.sh` 180 passed /
  0 failed; warnings 79 bin / 55 test. RED by mutation: restoring the loop makes the
  atomicity test fail with "no movement", because the FIRST bill commits and the second
  aborts — the defect reproduced as a failing assertion rather than described.
  **One trap found and now recorded in the code**: `resolve_account` reads through the
  POOL, so calling it inside a unit waits for the connection the caller already holds
  and answers `PoolTimedOut` on the `max_connections(1)` fixtures. It is a pre-check and
  it moved up with the others, before the unit opens.
- [ ] **P5 — The reads move to the per-document residual, and the legacy rows go with
  them.** `customer_balance`, `ageing_of`/`customer_ageing`/`ageing_all`, the
  `ENFORCE_CREDIT_LIMIT` projection, the `suppliers_web.rs:428` drawer fold,
  `outstanding_payables`, `customer_statement`: residual = `charge + returns of that
  document − Σ allocations to it`, unapplied credit from the payment documents. The
  document folds are **deleted**, not left dormant (decision a): the
  `create_payment_in` calls that feed them go in the SAME change, because a read that no
  longer asks for `sale_payments` is what retires it, and deleting the rows while a fold
  still read them is the failure this avoids. Two more things ride along: the mirror of
  `create_payment_in` for the four cancel paths disappears with the same decision, and
  `unapplied_for_party` (`Σ unapplied` over `direction='In'` deliveries only — an `Out`
  refund carries its whole amount as unapplied and would count as available credit) is
  what P6 needs for the saldo a favor. One fold cannot be deleted without a replacement:
  `payment_repo.rs:270` `target_residual_due` recomputes the document's own total for the
  allocation cap, and P5 re-bases it on the same residual the reads use instead of keeping
  a second copy of a total. A per-document `Return` fold is also wanted here, not in P6:
  the link exists (`customer_returns.sale_id` / `purchase_returns.purchase_id`, NOT NULL),
  so the residual is literal instead of a convention. **The residual must round the
  document total PER LINE through `services::line_taxes::tax_inclusive_total`, the way
  `SalesService::tax_split` (`src/services/sales.rs:277`) does** — a repo that sums
  `qty * price + tax` unrounded disagrees with the service for any line whose product is
  not already at 2dp, and that disagreement is exactly what the identity above forbids.
  `src/repositories/party_ledger_repo.rs:14` is the precedent for reusing that helper
  from a repository, and the batch form derives it once so a single-document caller and a
  set caller cannot disagree. Tests: a returned credit sale lowers
  balance, ageing and frees the credit limit; a payment applied to the oldest invoice ages
  on the oldest invoice and is not moved by a later credit; the supplier drawer reflects a
  confirmed return; and the identity `balance == Σ residuals + unapplied` holds on the
  four scoreboard cases of "P5 blocker".
- [ ] **P6 — UI.** Collect/pay screen: pick the party, the amount, the method, and
  the documents with their amounts (oldest-first prefilled, editable); the
  unapplied remainder shown as available credit; an explicit "apply credit"
  action; the saldo a favor visible on the party page and the statement; and
  **reassignment** — re-pointing an allocation at another invoice, which is the
  user's chosen mirror at the credit-note level (decision b). Reassignment is a write
  that DOES NOT EXIST: `UPDATE payment_allocations` appears in no Rust file and in no
  migration, while decision 3 promised it and migration 46 already built the
  `BEFORE UPDATE` guard waiting for it. Moving a share from document A to B must
  satisfy A's residual, B's residual and the payment's total in ONE unit, because a
  delete-then-insert would pass through a state where the money is applied twice.
  `MessageKey` + ES + EN. Playwright coverage; regenerate the visual baseline only
  with proof the diff is exactly what was intended.
- [ ] **P6 — UI.** Collect/pay screen: pick the party, the amount, the method, and
  the documents with their amounts (oldest-first prefilled, editable); the
  unapplied remainder shown as available credit; an explicit "apply credit"
  action; the saldo a favor visible on the party page and the statement.
  `MessageKey` + ES + EN. Playwright coverage; regenerate the visual baseline only
  with proof the diff is exactly what was intended.
- [ ] **P7 — Verification pass.** `cargo test --locked`, `cargo check --all-targets`
  (delta vs the 79/49 baseline explained), `git diff --check`, `scripts/e2e.sh`,
  and a real-binary pass over the four business flows of this document.
- [ ] **P8 — Retire the legacy tables.** The last slice, and it cannot come sooner:
  drop `sale_payments`, `purchase_payments`, `customer_return_payments`,
  `purchase_return_payments` and `customer_receipts` in one migration, **only once
  a grep proves nothing references them** — no SQL, no read, no fixture. It also
  re-bases `backfill_party_ledger` (party-ledger T1) off those tables and onto
  `payments`/`payment_allocations` plus the document families, which is a required
  edit to already-approved code, not an optional cleanup. Tests: the ledger
  backfill still reproduces balances from the new tables, and the dropped tables
  are gone from `sqlite_master`.

## P5 blocker — the journal entries do not close the identity (measured 2026-10-09)

P5 was declared a read change because "the ledger is already written by every path".
The ledger is written by every path, but what it writes does not close the identity
`balance == Σ residuals + unapplied`, which is this document's own acceptance
criterion and the reason the reads can move at all. Four holes, each with its
evidence. They are the reason P5 is P5a + P5b and not one read slice.

**An earlier revision of this section blamed the SIGN of `Refund` ("`signed_amount`
marks `Refund +`, so every customer refund raises the debt"). That was WRONG and is
corrected here.** `+` is right for a `Refund`: a refund entry is for money ARRIVING,
and the customer-side writer that uses it (`customer_return.rs:899`, the cancel of a
credit note, `direction='In'`) is money coming back in. The real defect is not the sign
but which events write an entry at all, and the two sides of one business event do not
agree about it.

1. **The legacy dual-write is still LIVE in production code, and P5b is what turns it
   off.** `create_payment_in` (`src/repositories/sale_repo.rs:1250`) is production
   code — `mod tests` starts at `:1574` — and it is still called from the cash leg of
   `confirm` (`src/services/sales.rs:1556`), from `link_delivery_payment_in`
   (`:1611`, which is the path `record_payment` and the receipt collection both
   take), from `src/services/purchases.rs:1441` and `:1614`, and from the two cancel
   paths at `src/services/sales.rs:1751`. The call sites say so: "The legacy row,
   until P5 moves the reads". `paid_and_due` (`src/services/sales.rs:300`) folds THOSE
   rows, which is why the old reads still answer correctly today and will stop the
   moment they are deleted. Deleting the rows without moving these reads, or moving
   the reads without deleting the rows, each leaves two homes for one fact.

2. **The same event is journalled differently on the two sides.** Money going back OUT
   over a cancelled sale writes **no entry at all** (`sales.rs:1950` passes `&[]` and
   the refund delivery adds nothing to the journal), while the mirror case — money
   coming back IN over a cancelled purchase — writes `Refund +` (`purchases.rs:1750`).
   So a `Refund` entry means "money arrived" on both sides, and money that LEFT is
   represented by silence, which no reader can distinguish from a delivery that was
   never journalled. Worse, when the cancelled document is a CREDIT NOTE its reversal
   DOES write `Refund +` (`customer_return.rs:899`) — the same event (money out) that a
   cancelled SALE records as nothing.

3. **`PartyEntryKind::Cancel` is written by no production path.** Its only writers are
   the repository's own tests (`party_ledger_repo.rs:805,831,853,1443`). Neither
   `cancel` (`src/services/sales.rs:1785`) nor its three siblings write a reversal of
   the `Charge`/`Return` of the document they annul — which is precisely the entry the
   category exists for. A voided `Charge` stays on the books forever.

   Together, 2 and 3 mean the annulment is only half-journalled, and the half that is
   missing is the one the balance depends on. The scoreboard, against the entry kinds
   as they are:

   | Sequence | Journal folds to | Truth |
   |---|---|---|
   | `k3_ac8`: 40 credit, collect 15 + 25, cancel | `40 − 15 − 25 = 0` ✓ | 0 — `sales.rs:5099` asserts it, and it passes because the refund writes nothing |
   | Cash 30, confirm, cancel | `30 − 30 = 0` ✓ | 0 |
   | 50 credit, collect 30, cancel | `50 − 30 = +20` ✗ | −30: the customer paid 30 and got it back, so the shop owes THEM |
   | 50 credit purchase, pay 30, cancel | `50 − 30 + 30 = +50` ✗ | −30: the supplier holds 30 of the shop's money |

   The third row is the one to read: the voided `Charge` (+50) and the returned money
   (30) are both absent, and they err in the same direction, so the figure is off by
   80% of the document. The first row passing is what hid this — a fully collected and
   fully refunded document is the ONE case where silence happens to be right.

4. **An applied credit is invisible to the journal.** Applying credit writes an
   allocation and, by decision 5, no cash movement and no entry — correctly, since no
   money moved. But `unapplied_for_party` as sketched is `Σ (payment.amount − allocated)`
   over that party's deliveries, and an `Out` refund also carries `amount` with an
   empty allocation vector, so its whole amount would count as available credit. That
   read must be restricted to `direction='In'`. With that restriction the identity is
   `balance == Σ residuals + unapplied`, and every applied credit satisfies it because
   the residual falls by the same amount the unapplied does.

**What this makes of the plan.** P5a is a journal task, not a read task, and it lands
before P5b. **Decision (user, 2026-10-09): the annulment gets a compensating entry.**
With the corrected diagnosis that decision means two writes, and the SECOND one is not
a sign change:

- A `Cancel` entry that reverses what the annulled document charged.
- A `Refund` entry for every refund delivery. **The purchase-cancel path already writes
  it (`purchases.rs:1750`); the sales-cancel path is the one that is missing it.** The
  supplier side is the template and the customer side is half-implemented, so "fix the
  refund's sign" from the first reading of this section is retired in favour of "write
  the entry the mirror path already writes".

Checked against the scoreboard: 40/40/0 − 15 − 25 − 40 + 40 = **0**; 50 credit with 30
collected → 50 − 30 − 50 + 30 = **0**; cash 30 → 30 − 30 − 30 + 30 = **0**; a credit
note whose parent sale never wrote a `Charge` → −12 + 12 = **0**. All four agree with
the business, and `k3_ac8` stays green rather than being relaxed.

**One sub-decision P5a must settle, and it is why `Cancel` is not a one-line change:**
`signed_amount` gives `Cancel` a FIXED negative sign, which reverses a `Charge`
correctly. But annulling a CREDIT NOTE has to reverse a `Return` (also negative), so its
compensating entry must be POSITIVE, and `Cancel` with a fixed sign cannot express that
without being handed a negative magnitude — which breaks the "amount arrives positive"
rule every writer in this family keeps. Either `Cancel` stays fixed-sign and an
annulment of a return document uses `Charge` instead, or `Cancel` becomes a signed delta
like `Adjust` (and `Adjust`'s own lesson in `AGENTS.md` — a negative delta is a
legitimate increase — is the precedent). P5a must pick one and write the reason at the
sign function, where the next reader will look for it.

**Two decisions from the user (2026-10-09) dissolved part of this problem and left a
larger one. Both are recorded here rather than in a private note, because the second
one is a product decision that a future reader would otherwise "restore".**

**(a) The legacy dual-write is DELETED outright, not retired in P8.** The development
database is wiped, so there is no old code worth keeping alive for a read nobody will
run. This is a REVERSAL of decision 8's staging (which kept the legacy rows as the
crutch the old reads lean on until P5b moved them) and it makes P5b simpler: the rows,
the `create_payment_in` calls that write them and the folds that read them go in ONE
change instead of two, because the first thing that breaks — `paid_and_due` — is the
very thing being replaced. P8 keeps only its other half: re-basing
`backfill_party_ledger` off the legacy tables. Note that `delete_draft` calls
`set_cancelled` while `cancel` on a Draft does exactly the same thing, so the two are
redundant with each other; the deletion has to pick one and keep it.

**(b) `cancel` CEASES TO EXIST as a business action: a Confirmed document is never
annulled, it is CORRECTED with a mirror document** — a credit note for a sale, a return
for a purchase. Only a Draft can be removed. So there is no annulment to journal, which
is what retires the `Cancel` sign sub-decision above: the mirror document already
writes its own `Return` (`customer_return.rs:697`, `purchase_return.rs:707`), and that
entry is the compensating side.

What (b) costs, measured, because it is much larger than the sign question it answers:
4 service methods (206 + 155 + 145 + 142 lines, `sales.rs:1785`, `purchases.rs:1634`,
`customer_return.rs:791`, `purchase_return.rs:806`), their routes (web + API), the
`ConfirmationPolicy` wiring in `documents_web.rs`, 7 templates, permissions
`sales.cancel` / `purchases.cancel` with their localization, the `Cancelled` status
variant (47 references), `cancel_reason` / `cancelled_at` columns (58 references), 92
cancel-related test functions, and every other service that must treat `Cancelled` as
reachable. That is not a P5 slice; it is its own feature with its own branch and its
own review.

**The third level is where (b) hits a wall, and it needs a decision before the work
starts.** A Confirmed CREDIT NOTE has no mirror: grepping `DebitNote` over `src/` and
`migrations/` returns nothing, so "correct it with a mirror document" has no document to
point at for a return family.

**Decision (user, 2026-10-09): the mirror at the third level is not a new document — a
credit note is corrected by REASSIGNING money, not by annulling it.** The credit note
takes goods back and correctly reduces what the parent sale charged; what can be wrong
afterwards is *where* the money sits. So the correction is re-pointing an existing
allocation at another invoice of the same customer, and when cash genuinely has to move
there is already a refund `Out` delivery for that. No fifth document family, and no
`cancel` in any of the four services. The two rejected ways out are kept below because
the reasoning is what a future reader will want:

| Way out | What a user does to undo a credit note | Consequence |
|---|---|---|
| **Reassign the money (CHOSEN)** | Re-points an allocation at another invoice | Needs the one write decision 3 promised and that does not exist yet — see below |
| A debit note document family | Issues one, which re-charges what the note credited | Biggest scope: a fifth document family in a repo whose own `AGENTS.md` says three families already mirror each other |
| Keep `cancel` ONLY for the two return families | Reverses the note through the current path | Leaves the asymmetry the user is removing: an annulment for a return and not for what it returns to |

The `Cancel` journal entry stays unneeded in all three: the mirror is either a document
that writes its own entry or, in the chosen shape, no document at all.

**Two findings this decision turns into P6 requirements, both measured:**

- **The link a per-document `Return` needs already exists.** `customer_returns.sale_id`
  and `purchase_returns.purchase_id` are `NOT NULL REFERENCES … ON DELETE RESTRICT`
  (`migrations/20240101000041_create_customer_returns.sql:46`,
  `.../20240101000040_create_purchase_returns.sql:49`), so "the returns of THAT
document" is already answerable without a new column. What is not written yet is the
  fold that reads it, and it is what makes the P5b residual literal
  (`charge + returns of that document − Σ allocations`) instead of a convention.
- **Reassigning does not exist and is not a template: it is a missing write.**
  `UPDATE payment_allocations` appears in NO Rust file and in NO migration; the trait
  offers only `allocate_in` (an INSERT). Decision 3 explicitly promised the opposite
  ("Reallocating is an `UPDATE` inside the owning unit") and migration 46 already built
  the guard waiting for it, `BEFORE UPDATE` trigger with its `id <> NEW.id` clause. P6
  needs that write, and it inherits the same cap: moving a share from document A to B
  must satisfy A's and B's residuals and the payment's total in ONE unit, because a
  delete-then-insert would pass through a state where the money is applied twice.

## Relationship to the party ledger

| Party-ledger task | Fate | Why |
|---|---|---|
| T2 — entry writes in the four `confirm` paths + lift the return caps | **stays as written** | charges and returns are independent of the payment document; the caps' debt side is exactly what T2 lifts |
| T3a — units + entries for the collection paths | **absorbed by P3/P4** | the cash movement moves from per-sale to per-delivery, so writing it first means writing it twice |
| T1 — the ledger's `backfill_party_ledger` | **re-based in P8** | it reads `sale_payments` / `purchase_payments` / the two return-payment tables, which P8 drops |
| T3b — units + entries for the four cancel paths | **absorbed by P3/P4** | the refunds become out-payments with their own account and one `Expense` |
| T4 — reads move to the ledger | **absorbed by P5** | the residual is per document, which needs the allocations |
| T5 — explicit credit application UI | **absorbed by P6** | applying credit is creating an allocation |
| decision 7 revised (per-document residual, FIFO only for the excess) | **superseded** | with allocations there is no excess to guess at: the operator allocates, the schema caps |

This is why payment-allocation must not be built after T3a: it *is* T3a's final
shape.

## Route declaration

| Task | Route | Trigger |
|---|---|---|
| P1 | delegated writer | migration + new repository file + tests |
| P2 | delegated writer | backfill + tests, same pattern as T1 |
| P3 | delegated writer | 3 service paths + lifted refusals + tests |
| P4 | delegated writer | mirrored supplier paths + tests |
| P5 | delegated writer | 6 read sites across 4 files, the legacy writes it deletes, and `unapplied_for_party` |
| P6 | delegated writer | route + template + JS + localization + e2e, plus the reassignment write |
| P6 | delegated writer | route + template + JS + localization + e2e |
| P7 | fresh verification worker | full suite + browser suite + real-binary flows |

Parent does: git state, feature-doc updates, per-task spot checks, delivery
decisions.

## Acceptance criteria

- One cash delivery covering N documents is **one** numbered document and **one**
  cash movement, on both the customer and the supplier side.
- The operator chooses which documents the money pays, with amounts; the system's
  suggestion is editable and never silently overrides him.
- The schema refuses to allocate more than the delivered amount, ever.
- A customer who pays more than owed leaves an unapplied remainder that is
  displayed as available credit and can be applied to a later document without any
  cash movement.
- An allocation can be corrected without moving money and without writing a ledger
  entry.
- The party balance is one fold over `party_ledger_entries`; the per-document
  residual is `charge + returns − allocations`; and the two agree:
  `balance == Σ residuals + unapplied`.
- Every money path is atomic: a failure leaves no payment row, no allocation, no
  cash row and no ledger entry.
- `cargo test --locked` and `scripts/e2e.sh` pass, with the warning delta explained.

## Delivery

- Forecast at creation: **~2.200–3.000 authored lines** across eight slices (P2
  cancelled, P8 added). Revised from the initial ~2.500–3.500 the same day the
  database wipe removed the backfill. Stated as a range because the last three units
  (T1 ~1.958, T1b ~200, T5 ~1.400) show this repo's authoring runs well above a
  line-count first guess.
- Strategy: Feature Branch Chain with a tracker, stacked on `feat/party-ledger`
  (P3 writes ledger entries, so it depends on T1). One draft/no-merge tracker PR;
  each child targets the previous branch; `main` only ever sees the integrated
  feature. Branch name proposal: `feat/payment-allocation`.
- Slice map: P1 (~500), P2 (~350), P3 (~500), P4 (~400), P5 (~450), P6 (~450),
  P7 (verification, few lines). Each PR body carries the dependency diagram with 📍
  on the current PR, its start/end, prior dependencies and out-of-scope items; tests
  and docs travel with the unit they verify. A slice over 400 after honest slicing
  is reported with a `size:exception` recommendation, never compressed.
- Push, PR creation and merge stay the user's decisions; local work-unit commits on
  the chain's branch are part of this authorized implementation.

## Progress

- 2026-10-03 — document created after the party-ledger review of the money model
  (Engram `architecture/payment-allocation`), one business decision from the user
  (a supplier payment may cover one or several invoices) and four design questions
  answered in conversation: shared family, form first, mutable allocations, schema
  cap. No code yet.
- 2026-10-08/09 — **P1, P3 and P4 implemented and pushed** over 20 commits; see the
  Status header for the measured state and "Resume here" for what P5 inherits.
- 2026-10-03 — **two decisions from the user revised the plan.** (1) The
  development database will be wiped, so nothing historical needs preserving: the
  payment backfill (P2) is cancelled, decision 8 now retires the legacy tables in a
  final cleanup slice (P8) instead of migrating them, and the only ugly concession
  of the design — "one delivery = one movement" not being retroactive — disappears
  with the history. (2) A fresh install must be able to collect money: a default
  `Caja` account with the seeded `Cash` method linked to it is seeded, tracked as T6
  of `odd/tasks/payment-method-single-account.md`.
- 2026-10-09 — **P5 was re-scoped and then simplified by two user decisions, before any
  code was written.** Exploration measured that the ledger, though written by every
  path, did not close `balance == Σ residuals + unapplied` (no production path wrote
  `PartyEntryKind::Cancel`; the sales-cancel refund wrote no entry while its
  purchase-cancel mirror wrote one; an applied credit is deliberately invisible to the
  journal; the legacy rows were still written on purpose). A first revision of "P5
  blocker" blamed the SIGN of `Refund` and was WRONG — the correction is recorded in
  place, because the wrong version is the one a future reader re-derives. The user then
  (a) deleted the legacy dual-write outright instead of retiring it in P8, since the dev
  database is wiped, and (b) retired `cancel` as a business action entirely, a Confirmed
  document being corrected with a mirror document. Together those removed the journal
  blocker, so P5 is one task again and `cancel`'s retirement is a separate feature. A
  third decision closed the wall a Confirmed credit note hit: it is corrected by
  REASSIGNING money, not by annulling it. Nothing was implemented; the two stacked
  branches (`feat/party-ledger-p5`/`-p6`) were dropped for one PR on `feat/party-ledger`.
- 2026-10-09 — **P5.1 and P5.2a landed** (commits `6c4ea0f` and the batch read). The
  residual now has ONE definition: the batch form (`residuals_for_documents`, three
  queries for N documents, non-empty input) and the single-document form both go through
  the same arithmetic, so a set caller and a one-document caller cannot disagree. Two
  corrections were needed along the way and both are worth keeping in mind:
  (1) the repo summed `qty * price + tax` per line UNROUNDED while
  `SalesService::tax_split` rounds each line through `line_taxes::tax_inclusive_total`;
  they diverge for any line whose product is not already at 2dp (`0.333 x 3` twice is
  `2.00` one way and `1.998` the other), and that divergence is exactly what
  `balance == residuals + unapplied` forbids. The helper import is the precedent set by
  `party_ledger_repo.rs:14`. (2) `cargo check` sits at **80 bin / 55 test**, one above the
  79 baseline, and that one warning is EXPECTED AND CORRECT: `allocated_to_target_raw` is
  reported as never used because its only caller is the `allocated_to_target` trait
  method, which itself has no production caller yet — the tests reach it, which keeps it
  out of the bin's dead-code analysis only in the test target. This is the transitive
  dead-code shape `AGENTS.md` describes: the count will drop when P5.2b wires the read
  sites to this layer, and that drop is the EVIDENCE the wiring is real. Do NOT silence
  it with `allow(dead_code)` and do NOT delete the helper: deleting it breaks
  `allocated_to_target`, which P6 needs.

## Resume here
  code was written.** Exploration measured that the ledger, though written by every
  path, did not close `balance == Σ residuals + unapplied` (no production path wrote
  `PartyEntryKind::Cancel`; the sales-cancel refund wrote no entry while its
  purchase-cancel mirror wrote one; an applied credit is deliberately invisible to the
  journal; the legacy rows were still written on purpose). A first revision of "P5
  blocker" blamed the SIGN of `Refund` and was WRONG — the correction is recorded in
  place, because the wrong version is the one a future reader re-derives. The user then
  (a) deleted the legacy dual-write outright instead of retiring it in P8, since the dev
  database is wiped, and (b) retired `cancel` as a business action entirely, a Confirmed
  document being corrected with a mirror document. Together those removed the journal
  blocker, so P5 is one task again and `cancel`'s retirement is a separate feature. A
  third decision closed the wall a Confirmed credit note hit: it is corrected by
  REASSIGNING money, not by annulling it. Nothing was implemented; the two stacked
  branches (`feat/party-ledger-p5`/`-p6`) were dropped for one PR on `feat/party-ledger`.

## Resume here

1. Read decisions 1–9 above before touching anything: three of them (2, 8, 9) are
   the ones a future agent will otherwise "improve" back into the old shape. Decision
   4 was REFINED (the cap has two homes; a payment's amount freezes once allocated)
   and the refinement is measured, not argued.
2. **P5 is the next unit again, and it is ONE task: the reads, the legacy deletion and
   `unapplied_for_party`.** Read "P5 blocker" before starting: the journal blocker was
   real, and the two user decisions of 2026-10-09 dissolved it rather than leaving it to
   P5. The legacy dual-write is DELETED, not retired in P8 — which is why P5 and that
   half of P8 are the same change: the first fold to break when the rows go is the one
   being replaced. `unapplied_for_party` must filter `direction='In'`, or an `Out` refund
   counts its whole amount as available credit. `target_residual_due`
   (`payment_repo.rs:270`) also has to be re-based here, since it keeps a second copy of
   the document's total.
3. **`cancel` is being RETIRED as a business action (user, 2026-10-09), and that is its
   own feature, NOT part of P5.** A Confirmed document is corrected with a mirror
   document; only a Draft is removed. Measured scope, so it is not underestimated: 4
   service methods (206 + 155 + 145 + 142 lines), their web and API routes, the
   `ConfirmationPolicy` wiring, 7 templates, the `sales.cancel`/`purchases.cancel`
   permissions with their localization, 47 references to the `Cancelled` variant, 58 to
   `cancel_reason`/`cancelled_at`, and 92 cancel-related test functions. It cannot land
   before P5, because P5 is what proves the money model it leans on, and it removes the
   last journal hole rather than adding to it. `delete_draft` and `cancel`-on-a-Draft do
   the same thing today (`customer_return.rs:811`), so the work picks one.
4. **Expect three or four tests to fail on purpose when P5 lands, and they are not
   regressions.** They were written as the reminder: the most visible is
   `over_collection_becomes_the_customers_credit` (`src/services/customer_receipts.rs:1374`)
   asserting `customer_balance == 0` with a comment saying P5 will make it `-1`, and
   the same caveat in `src/routes/customers_api.rs:1147`. Read the assertion message
   before "fixing" it. Conversely `k3_ac8_fully_paid_and_cancelled_sales_leave_the_balance`
   (`src/services/sales.rs:5099`) already asserts the RIGHT answer for an annulled
   document, so P5 must keep it green rather than relax it — and it will be re-derived
   once `cancel` is retired, because the fixture that builds it stops existing.
4. **P8 is not a cleanup**: `backfill_party_ledger` (party-ledger T1, reviewed with its
   authority burned) READS the five legacy tables, so dropping them requires re-basing
   it onto `payments`/`payment_allocations` or the startup fails with "no such table".
5. The two refund caps of party-ledger T2 (`customer_return`, `purchase_return`) are
   still NOT lifted, deliberately: lifting them needs a decision nobody has written
   ("what replaces *this app has no credit balance*"). See T2 in
   `odd/tasks/party-ledger.md`.
