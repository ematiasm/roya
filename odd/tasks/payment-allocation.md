# Payment allocation — one delivery of money, many documents

## Status

**Planned.** The design decisions are settled (all dated 2026-10-03); no code
written yet. This document **reshapes** tasks T3a, T3b, T4 and T5 of
`odd/tasks/party-ledger.md` — see "Relationship to the party ledger". T1, T1b and
T2 of that document are untouched by this one.

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
  **PARTIALLY DONE 2026-10-08: P3a and P3b are in, P3c and P3d are not.** The two
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
  * Together: `cargo test --locked` 1543 passed, `scripts/e2e.sh` 180 passed,
    warnings 82 bin / 53 test (a P1 peak of 97 → 82 as the layer got called).

  The original text follows, unchanged, as the remaining contract. `collect`,
  `record_payment`, and the customer cancel/refund paths: one `payments` row, N
  allocations, one `transactions` row, one `Payment` ledger entry, all inside the
  caller's unit; explicit allocation input (`[(sale_id, amount)]`) with the
  oldest-first plan offered as a default; the two customer overpayment refusals
  lifted. Tests first (RED): a failure injected between the cash row and the
  payment row leaves neither; collecting 3 invoices with one amount writes **one**
  `Income`; overpaying leaves `unapplied > 0` and no error; applying that credit to
  a later sale writes an allocation and **no** cash movement.
- [ ] **P4 — Supplier money paths, mirrored.** `pay_supplier`, purchase
  `record_payment`, purchase cancel/refund: one `payments` (`direction='Out'`) with
  N allocations. Same tests mirrored, plus: one handover of 200.000 covering four
  bills is one document and one `Expense`.
- [ ] **P5 — Reads move to the per-document residual.** `customer_balance`,
  `ageing_of`/`customer_ageing`/`ageing_all`, the `ENFORCE_CREDIT_LIMIT`
  projection, the `suppliers_web.rs:428` drawer fold, `outstanding_payables`,
  `customer_statement`: residual = `charge + returns of that document − Σ
  allocations to it`, unapplied credit from the payment documents. The document
  folds are **deleted**, not left dormant. Tests: a returned credit sale lowers
  balance, ageing and frees the credit limit; a payment applied to the oldest
  invoice ages on the oldest invoice and is not moved by a later credit; the
  supplier drawer reflects a confirmed return.
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
| P5 | delegated writer | 6 read sites across 4 files |
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
- 2026-10-03 — **two decisions from the user revised the plan.** (1) The
  development database will be wiped, so nothing historical needs preserving: the
  payment backfill (P2) is cancelled, decision 8 now retires the legacy tables in a
  final cleanup slice (P8) instead of migrating them, and the only ugly concession
  of the design — "one delivery = one movement" not being retroactive — disappears
  with the history. (2) A fresh install must be able to collect money: a default
  `Caja` account with the seeded `Cash` method linked to it is seeded, tracked as T6
  of `odd/tasks/payment-method-single-account.md`.

## Resume here

1. Read decisions 1–9 above before touching anything: three of them (2, 8, 9) are
   the ones a future agent will otherwise "improve" back into the old shape.
2. P1 is the next unit: migration 46 + the repository + the two guards, tests
   first. It starts unwired on purpose, so the warning count rising is expected
   evidence, not a regression.
3. Before writing P3, re-read party-ledger decisions 4, 5 and 6: lifting the
   overpayment refusals and applying credit are the same change as allocations.
