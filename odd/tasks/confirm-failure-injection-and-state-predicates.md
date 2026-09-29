# Confirm failure injection and state predicates

## Status

T1 through T4 implemented and verified, not yet committed. Branch
`test/confirm-failure-injection-and-state-predicates`, stacked on `89defb3`
(`fix/sales-stock-precheck-demand-sum`), which holds the closed pre-check fix.

4 files, +1192/-8. `cargo test` → **1278 passed, 0 failed** (baseline 1263, +15 new).
`cargo check --all-targets` → 0 errors, 83 warnings, identical to the stashed pre-change
baseline. `cargo fmt --check` and `git diff --check` clean. `scripts/e2e.sh` not applicable:
no template, static file or route is touched.

T2 as originally specified was **wrong about the code** and was corrected — see Decision 6.

## Objective

Prove the partial-write windows of `confirm` with real tests, and close the one predicate
that is safe to add. Neither unit makes `confirm` atomic. The shared transaction does that,
and it is not in this document — see "Still open".

## What already works — this bounds the work

Read from the tree, not inferred:

- The strict stock pre-check already aggregates the demand per product
  (`src/services/sales.rs:1306`, closed in `89defb3`). Do not re-litigate it here.
- The team already owns the state-predicate pattern: `delete_line` uses
  `WHERE id = ? AND EXISTS (... status = 'Draft')` plus a `rows_affected() == 0` refusal
  (`src/repositories/sale_repo.rs:817-828`), and `delete_draft` uses a WHERE predicate
  (`sale_repo.rs:844-853`). The confirm path does not.
- The failure-injection technique is proven and in use: `sqlx::raw_sql` plus
  `CREATE TRIGGER ... BEGIN SELECT RAISE(ABORT, ...)`, at
  `src/tax_snapshot_tests.rs:747-754` and `src/smoke_tests.rs:3505-3567`.
- `check_payment_links_are_traceable` (`src/smoke_tests.rs:2406`) already detects an orphan
  Income — but only inside the test module, with no runtime equivalent.

## The problem

`SalesService::confirm` (`src/services/sales.rs:1186`) and `PurchasesService::confirm`
(`src/services/purchases.rs:959`) write in the order sequence -> stock -> finance -> payment
-> document, each on its own autocommit connection. There is no shared transaction. Every
statement is individually atomic; the sequence is not.

1. `set_confirmed` has no status predicate (`sale_repo.rs:715`, `purchase_repo.rs:600`), so a
   duplicate submission writes a second time instead of being refused.
2. `create_payment` has no state gate (`sale_repo.rs:921`, `purchase_repo.rs:828`; the trait
   declarations are at `:248` and `:234`). Measured, not inferred: it has four callers, plus
   two in `t1_schema_tests.rs`.
3. No test injects a failure into `confirm` on either service. A repo-wide grep for injected
   abort triggers returns hits only in `tax_snapshot_tests.rs`, `smoke_tests.rs` and
   `customer_receipts.rs` — none in either service's test module.
4. The atomicity note at `sales.rs:17-22` claims the only expected side effect of a failure
   after validation is a sequence gap. That is false: partial stock movements, orphan
   finance rows and a payment on a Draft are all reachable. **Now empirically disproven** —
   T3 and T4 are tests that fail if anyone claims the note is true again. The note itself is
   still in the source and still wrong; correcting it is outstanding.
5. `record_cost` runs in a loop **after** `set_confirmed` (`purchases.rs:1168-1178`), so a
   failure there is not a residue on a Draft. It is a residue on a **Confirmed, numbered,
   fully paid** purchase. This window was not in the original plan; see T4 and Decision 7.

## The problem — measured

The residue each window leaves, as asserted by T3 and T4. These are observed, not predicted.
Sales ran with strict stock on (`svc_with_flags(false, false)`); purchases ran with
`allow_balance = true`, which is required or every Cash confirm is refused against a fresh
empty account and the trigger never fires.

| Window | status | number | movements | transactions | payments | costs |
| --- | --- | --- | --- | --- | --- | --- |
| W1 number → 1st movement | `Draft` | NULL | seed only | 0 | 0 | — |
| W2 2nd of two movements | `Draft` | NULL | **+1 committed Out, qty 2** | 0 | 0 | — |
| W3 finance → payment | `Draft` | NULL | full | **+1 orphan Income/Expense** | 0 | — |
| W4 `set_confirmed` | `Draft` | NULL | full | +1 | +1 | — |
| **W5 `record_cost`** (purchases) | **`Confirmed`** | **set** | full | +1 | +1 | **1 of 2** |

The sequence is spent in every window: `sale_sequence_last` is `Some(1)` throughout.

Two of these are worse than a gap:

- **W4** leaves a Draft that `get_detail` reports as `payment_status = Paid`, `paid = 20`,
  `due = 0`. The shop's books show a full collection against a document still editable as a
  Draft.
- **W5** leaves a Confirmed, numbered, fully paid purchase with one of two supplier costs
  recorded, and it is **unrecoverable through the service**: the retry is refused by both the
  opening read and the new predicate. Unlike W1–W4, nothing the operator can do repairs it.

## Decisions

1. **Decided 2026-09-29 — the `set_confirmed` predicate is in scope; the `create_payment`
   gate is not.** `create_payment` has four callers: `sales.rs:1393` (confirm),
   `sales.rs:1491` (`record_payment_with_receipt`, the standalone collection flow reached
   from `routes/sales_api.rs:210` with `Require<CustomersCollect>`), `purchases.rs:1148`
   (confirm) and `purchases.rs:1239` (standalone). The standalone flows run against an
   already-Confirmed document, so gating the insert on `status = 'Draft'` would refuse the
   collection of a confirmed credit sale.

2. **Decided 2026-09-29 — the predicate is not a state.** No migration, no CHECK change, no
   `SaleStatus` variant. `status` keeps its three values
   (`src/models.rs:1166-1170`); the statement gains a precondition. A state is a value a
   document rests in; a predicate is a condition on a transition.

3. **Decided 2026-09-29 — the predicate does not stop the partial-write retry, and this
   document must not claim it does.** A failed attempt leaves the document in Draft, so
   `status = 'Draft'` is true on the retry and the second attempt writes again. What the
   predicate refuses is a duplicate submission of a document that has already been
   confirmed. This is the same reason Odoo needs no payment predicate: its whole request is
   one transaction, so a failed attempt rolls back whole and a retry starts clean.

4. **Decided 2026-09-29 — the unit of the transaction is the user action, not the repository
   method.** This is Odoo's structural answer (`odoo/service/model.py::retrying` in 19.0:
   one cursor per request, commit at the end, re-raise past the commit on error) and it also
   removes the sequence gap for free, because `UPDATE last_number = last_number + 1` is
   already a transactional write.

5. **Decided 2026-09-29 — keep refusing on insufficient stock; do not copy Odoo's
   reservation model.** Odoo never pre-checks: `_get_reserve_quantity` takes
   `min(quantity, available_quantity)` and a short line becomes `partially_available`, a
   state rather than an error. For a shop the owner must be told they cannot sell, not
   silently shorted. The pre-check-and-refuse is the right shape here.

6. **Decided 2026-09-29 — T2's original RED was wrong about the code, and the gap it names
   is TOCTOU, not a sequential double confirm.** This document previously asserted that
   "confirm a sale, then confirm it again" writes a second movement and a second payment. It
   does not. `sales.rs:1197-1199` and `purchases.rs:970-972` already refuse a `Confirmed`
   document at the opening read, and `ac6_double_confirm_and_edit_confirmed_rejected`
   (`sales.rs:2393`) already covers it. The test written to the original spec **passed from
   birth**.

   The real reachable case is that the opening read is not a lock: between it and the
   `set_confirmed` write, another writer can confirm the same document, and without the
   predicate the losing writer stamps a second number over the first and reports a success
   the caller must never be told about. T2 was rewritten to force that window
   deterministically with a trigger that runs a second writer inside the statement.

   A genuine two-connection race is **not testable in this harness**: `test_pool()` is
   `max_connections(1)`, which serialises the window out of existence by construction. The
   predicate is proved by induction on the statement, not by observed concurrency.

7. **Decided 2026-09-29 — W5 is its own problem, separate from the Draft residues.** Because
   `record_cost` runs after `set_confirmed`, its failure is invisible to the predicate: the
   document is legitimately Confirmed, so there is nothing for a `status = 'Draft'` gate to
   refuse, and the retry is correctly refused by the opening read. The window cannot be
   closed with a predicate at all. It is closed by the shared transaction, or by moving
   `record_cost` inside one.

8. **Decided 2026-09-29 — the predicate also closed a defect that was not in the plan.**
   Before it, `set_confirmed` called directly on a **Cancelled** sale returned `Ok` and
   resurrected it: `status` became `Confirmed`, `cancel_reason` was wiped to `None`, and
   `cancelled_at` stayed set. A second direct call overwrote an already-assigned number
   (`…000001` → `…000002`). Both are refused now. This is worth stating plainly: the
   repository method was not enforcing the state machine it belonged to.

## Tests that must change, and how

None existing. This work is additive.

## Tasks

- [x] **T1 — `set_confirmed` refuses a document that is not a Draft.** Shipped at
  `sale_repo.rs:747` and `purchase_repo.rs`, with `execute` + `rows_affected() == 0` and a
  new `refuse_confirm()` that reads the state back and names it, matching the existing
  `refuse_line`. `execute` + read-back was chosen over `fetch_optional` on `RETURNING` for
  consistency with `delete_line`, `delete_draft` and `write_line_with_taxes`, which all end
  the same way; the cost is one extra read, and the failure mode is a loud `NotFound`, not
  a silent one. **RED**: 4 failures, `unwrap_err()` on an `Ok` value — including the
  Cancelled sale resurrected as `Confirmed` with `cancel_reason: None` and `cancelled_at`
  still set, and an assigned number overwritten `…000001` → `…000002`. The doc comment at
  `sale_repo.rs:753-767` states what the predicate refuses and, in the same comment, what it
  does not.
- [x] **T2 — a duplicate confirm submission is refused end to end.** **Corrected while
  being implemented; see Decision 6.** The original spec was a sequential double confirm and
  it passed from birth, because the opening read already refuses a Confirmed document. T2
  now forces the TOCTOU window with a trigger that runs a second writer inside the
  statement. RED showed the losing confirm returning `Ok` and overwriting the winner's
  `2024-SALE-000099` with its own `…000001`. GREEN: refused, with movement, transaction and
  payment counts unchanged.
- [x] **T3 — every failure window of `SalesService::confirm` has a test that names its
  residue.** Four tests, all passing on first run **by design** — these are characterization
  tests, not TDD, and their deliverable is the residue table above. The instruction to the
  implementer was explicit: do not attempt to remove the residue, and never weaken an
  assertion to make it pass. W2's committed `Out` is asserted by reading `qty` off the row,
  not inferred from the count. W3's orphan is asserted as `kind = "Income"`,
  `reference = "2024-SALE-000001"`, `amount = "20"` — it carries the burned number and the
  full document total while the sale is a Draft nobody can reconcile.
- [x] **T4 — the same four windows for `PurchasesService::confirm`, plus W5.** Five tests,
  all passing on first run by design. W5 is the one that matters: injecting a failure into
  the second `record_cost` leaves a **Confirmed, numbered, fully paid purchase with 1 of 2
  costs recorded**, and the test proves it is permanent — `find_cost(second, supplier)` is
  `None` and a retry is refused with `"purchase already confirmed"`, leaving the count at 1.
  See Decision 7 for why no predicate can close it.

One assertion was corrected rather than weakened, and the correction sharpened it.
`confirm_failure_on_set_confirmed_leaves_a_paid_draft` first asserted
`payment_status == Unpaid`; it failed with `left: Paid, right: Unpaid`. The assertion was
wrong about the code, not the other way round — `get_detail` computes `paid` from
`sale_payments`, and that row had been committed. It now asserts the true and stronger fact:
the Draft reports itself `Paid`, `paid == 20`, `due == 0`.

## Constraints

- Do not add a `status` value. The `CHECK (status IN ('Draft', 'Confirmed', 'Cancelled'))`
  in migrations 08, 15, 21, 32 and 33 makes any new state a full table rebuild in SQLite.
- Do not gate `create_payment` on the document's state. See Decision 1.
- Do not change the mutation order, the refusal messages, or any arithmetic.
- Do not add a compensating-transaction or saga layer. Nothing today can reverse a finance
  row by `reference`.
- T3 and T4 assert the CURRENT residue. Do not weaken them to "an error was returned" — the
  residue assertion is the entire value of the test. Fixing the atomicity is out of scope;
  if a characterization test fails, the assertion is wrong about the code, and it must be
  corrected to the true behaviour and the correction reported, not smoothed over.
- Do not "fix" a T3/T4 residue that looks like a bug. It is a bug, but it is the shared
  transaction's bug, and the tests exist to keep it visible until then.

## Applicable checks

- Strict TDD: observe RED before GREEN, for T1 and T2, and report the literal output. T3 and
  T4 are characterization tests and pass by design; their RED is the residue itself.
- `cargo test` — the full suite. Record the pass count and the baseline.
- `cargo check --all-targets` — 0 errors, and a warning count not worse than the baseline
  measured through a stash.
- `cargo fmt --check` and `git diff --check` clean.
- No template, static file or route is touched, so `bash scripts/e2e.sh` is not applicable.
  Do not run it. Same for `cargo clippy`: this project has no clippy gate
  (`.github/workflows/checks.yml:5-10`).

## Outcome

T1–T4 implemented and verified. Not yet committed.

The predicate closes a duplicate submission, and it also closed a defect nobody had planned
for: `set_confirmed` no longer resurrects a Cancelled sale or overwrites an assigned number.
That is a real win and it was free.

It does not make a failed confirm safe to retry, and nothing here pretends otherwise. The
tests now measure exactly what a partial confirm leaves, in both services, in five windows —
and one of those windows is worse than anything predicted: a purchase that is Confirmed,
numbered, fully paid, missing a supplier cost, and unfixable through the service.

The atomicity note at `sales.rs:17-22` is now contradicted by the test suite. It is still in
the source. **Correcting it is the next smallest honest step in this document's story**, and
it is not optional: a note that says "only a sequence gap" is a note that will mislead the
next person who reads the code before reading the tests.

## Still open — not in this document

- **No shared transaction.** This is the actual fix, and nothing in T1–T4 substitutes for it.
  It also removes the sequence gaps for free, because `UPDATE last_number = last_number + 1`
  is already a transactional write.
- **W5, the `record_cost` window, is unrecoverable.** See Decision 7. It is listed here
  separately from the Draft residues because no predicate can reach it, and because it is the
  one residue where the operator has no path forward at all.
- The orphan `Income`/`Expense` window: a failure between the finance row and the payment row
  leaves a `transactions` row with a `reference` and no parent document, and no service method
  can reverse it.
- `delete_draft`'s documented premise (`sale_repo.rs:840-843`, "a draft never touched stock,
  money") is falsified by a partial confirm. W4 makes it concrete: the Draft in W4 has a
  payment row, and `delete_draft` will cascade it away while the `Income` survives.
- Gaps are burned silently. Odoo tracks them: `account.move` carries a stored
  `made_sequence_gap` boolean with a surface that reports holes. We have neither.
- A genuine two-connection race is untestable while `test_pool()` is `max_connections(1)`.
  Everything proved here is proved by induction on the statement.
