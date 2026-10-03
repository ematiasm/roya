# Party ledger — cuenta corriente por tercero

## Status

**In progress.** T1 delivered and committed as `0dfe5c1` on `feat/party-ledger`;
T2, T3a, T3b, T4, T5, T6 pending. T1's native review is granted-but-blocked — see
Progress and Resume here. This document is itself an uncommitted change
(`M odd/tasks/party-ledger.md`) on purpose.

## Objective

One ledger of signed entries per party (customer, supplier) that becomes the single
source of truth for every party balance in the app: outstanding debt, saldo a favor
(credit), ageing, credit limit, payables, and the account statement.

**What this is, and what it is not.** It is a *party subledger*: a signed journal
per third party. It is **not** a general ledger. There is no contra-account, so it
carries no verifiable balance invariant of the kind "every debit has an equal and
opposite credit" — a party total is correct because every writer is correct, not
because the schema forces a double entry. Summing a party answers "what does this
third party owe, or hold, in saldo a favor"; it never answers "where did that money
come from". A profit-and-loss statement or a tax-by-account report would need a
real double-entry layer, which this application does not have and this feature does
not build. The scope below is written against that ceiling, so nobody reads a party
balance as more than it is.

It fixes the bug the user reported — **a confirmed return does not reduce the party
balance** — by construction rather than by patching each fold, and it closes open
item #1 of `odd/tasks/purchase-returns-and-credit-notes.md:315` ("the credit-balance
case … needs a decision, not a workaround").

## Problem

Party balances are derived folds that read only the parent document family:

- Supplier balance: `Σ money.due` over confirmed purchases
  (`src/routes/suppliers_web.rs:428-445`). `purchase_return_payments` is read by
  nothing outside its own repo and tests.
- Customer balance: confirmed credit sales minus `sale_payments`
  (`src/repositories/sale_repo.rs:783-795`, folded at `src/services/sales.rs:871`).
  Customer returns are invisible to it, to ageing, and to the credit-limit check.
- Cash accounts ARE a ledger (`transactions` + `signed_amount`), but credit
  movements are never written anywhere: a credit sale/purchase confirm writes no
  finance row at all (`src/services/sales.rs:1447`, `src/services/purchases.rs:1197`).

Three consequences: returns silently leave the balance wrong; a return whose parent
collected less than the return total is *refused* (`src/services/purchase_return.rs:560`,
`src/services/customer_return.rs:556`) because there is nowhere to hold the
difference; and overpayment is refused in four places because a party balance can
never legitimately be negative.

## Why level 2 and not the surgical fix

The surgical fix (net returns into the parent document's `due`) fixes the reported
bug but leaves five independent folds computing the same concept, leaves no home for
a credit balance, and leaves the payment/cancel paths without a transaction. The
ledger is the base that makes all four correct at once.

## Decisions

1. **Decided 2026-10-03 — one signed entry table, one sign rule for both sides.**
   `amount > 0` means *outstanding obligation*: the customer owes the business, or
   the business owes the supplier. `amount < 0` is the saldo a favor. The rule set is
   identical for both party types:

   | Event | Entry kind | Sign |
   |---|---|---|
   | Document confirmed (credit or cash) | `charge` | `+total` |
   | Cash settled against the document | `payment` | `−amount` |
   | Goods returned (return / credit note) | `return` | `−total` |
   | Cash handed back to the party | `refund` | `+amount` |
   | Whole document annulled | `cancel` | `−total` |

   Worked examples: customer owes 200 and pays 250 → `+200 −250 = −50` (credit).
   Business owes supplier 200 and pays 250 → `−50`. A credit sale returned in full
   before any payment → `+100 −100 = 0`. A cash sale of 100 cancelled →
   `+100 −100 −100 +100 = 0`.

2. **Decided 2026-10-03 — cash documents write their entries too.** A confirmed cash
   sale writes `charge +total` and `payment −total` in the same unit. Net zero, but
   the journal is complete, which is what makes an `extracto de cuenta` possible and
   makes cancel uniform across cash and credit.

3. **Decided 2026-10-03 — the journal never deletes, it appends reversals.** A
   cancelled document writes its own `cancel`/`refund` entries; no entry row is ever
   updated or removed after creation.

4. **Decided 2026-10-03 — overpayment is allowed and becomes saldo a favor.** The
   four overpayment refusals are lifted (`src/services/sales.rs:1553`,
   `src/services/purchases.rs:1296`, `src/services/customer_receipts.rs:110`,
   `src/services/purchases.rs:1378`). A negative balance is a legal state, not an
   error. The cash-account negative guard (`ALLOW_NEGATIVE_BALANCE`) is untouched:
   it guards cash boxes, a different axis.

5. **Decided 2026-10-03 — the return cap is replaced by the ledger.** Cash leaving
   the box stays capped by `refund_plan` at what the parent collected (an anti-fraud
   rule that stands on its own); the *debt* side is unbounded beyond the parent's
   own total. The `return is worth X but … only collected Y` refusals go away.

6. **Decided 2026-10-03 — credit is applied explicitly (user choice).** The balance
   available is displayed on the collection screen; the operator applies it with an
   explicit action before collecting the remainder. No silent netting of cash
   collection.

7. **Decided 2026-10-03 — reads come from one place; REVISED the same day for
   ageing.** `balance_for_party` (checked fold over entries) replaces
   `customer_balance`, the supplier drawer fold, and `outstanding_payables`.
   Ageing does **not** allocate the net balance FIFO across documents. Each entry
   already names its own document (`document_kind`/`document_id`), so the residual
   of one document is exact — its `charge` plus its `payments` — and FIFO covers
   **only** the net excess that has no document left to land on (a credit balance
   remaining after every confirmed document of the party is settled). A payment the
   operator applied to the oldest invoice stays on the oldest invoice, which is the
   fact the old per-document fold carried and a pure FIFO reconstruction throws
   away. Cost: the same one full read plus a group-by.

   The one thing the ledger does not carry is the *parent* of a return: a `Return`
   entry names the return document it is a movement of, not the sale or purchase it
   reduces. Attributing it to the parent therefore reads the parent link the return
   family owns (`customer_return_lines.sale_line_id` → `sale_lines` → the sale;
   `purchase_return_lines.purchase_line_id` → `purchase_lines` → the purchase),
   the same link AGENTS.md says points at the parent LINE and never at a
   product. That is a read-side join in T4, not a new column: if the join ever turns
   out to be the wrong home for the fact, the alternative is a `parent_document_id`
   on the entry, and that is a schema decision to raise then.

8. **Decided 2026-10-03 — every entry is written inside the unit that owns the
   event.** The four `confirm` paths already have one; `record_payment`,
   `pay_supplier`, `collect` and the four `cancel` paths open one (they currently
   write autocommit rows on separate connections — a latent bug of their own).

9. **Decided 2026-10-03 — backfill runs in Rust, immediately after
   `sqlx::migrate!`; revised the same day after reading migration 39.** Migration 42
   creates the table and indexes only. It cannot seed it: a document total is
   *derived* — `round_half_up(qty * unit_price + tax_total, 2)` per line
   (`migrations/20240101000039:27-30`) — and SQLite arithmetic over TEXT decimals
   converts to REAL, which is exactly the floating point this project forbids for
   money. So the backfill is one Rust function beside the repository it fills,
   called from `db.rs` right after the migrate call, inside one transaction, guarded
   by an empty `party_ledger_entries` (a no-op either way on a fresh database).
   It uses the same pure money functions the old folds used, so post-backfill
   balances equal the old derived folds **plus** the returns correction.

10. **Decided 2026-10-03 — the database enforces append-only, not the prose.**
    Decision 3 was a convention until migration 43: `BEFORE UPDATE` and
    `BEFORE DELETE` triggers on `party_ledger_entries` abort every statement with
    the trigger's own message, so "no row is edited or deleted" is a property of
    the schema a future writer, screen or script cannot route around. The two
    vestigial columns go with it: `updated_by` and `updated_at` are dropped by
    migration 43, **superseding migration 42's note** that they exist "for a
    future correction path". A correction is a reversal entry (decision 3), so a
    timestamp nobody may write is dead schema inviting the very mutation the
    trigger refuses. Migration 42 itself is left byte-identical: `sqlx::migrate!`
    verifies the checksum of every applied migration, so editing it — even a
    comment — breaks every existing database at startup.

11. **Decided 2026-10-03 — a single-instance kind cannot be written twice.**
    A partial unique index (`WHERE kind IN ('Charge', 'Return', 'Cancel')`) over
    `(document_kind, document_id, kind)` makes a duplicated confirmation, return
    or cancellation a database error instead of a silent balance change — the
    cheapest form of the idempotency key a ledger is expected to carry. `payment`
    and `refund` are deliberately outside it: several of each legitimately settle
    one document. Together with decision 10 this makes the two integrity rules
    that the plan review found missing (see Engram `review/party-ledger-design-gaps`)
    enforceable rather than remembered.

12. **Decided 2026-10-03 — the journal only ever references a CONFIRMED document,
    which is what closes the orphan risk the missing FK leaves open (no new
    trigger).** A Draft writes nothing — no stock movement, no finance row, and
    therefore no ledger entry — and, precisely because nothing points at it, a
    Draft is the one thing that is deletable. Only a confirmation mints a document
    number and puts the document into the records that get counted. Two
    consequences, and they are requirements on T3b rather than code to add now:
    (i) a document cancelled while still a Draft was **never** confirmed, so its
    cancellation appends **no** entries — a `cancel`/`refund` pair over a document
    that never carried a `charge` would invent a movement, and would also make a
    document that `delete_draft` still admits (`sale_number IS NULL`,
    `src/repositories/purchase_repo.rs:2162`) a deletable document *with* entries;
    (ii) since `delete_draft` refuses anything that is not a Draft, a document with
    entries can never be deleted, so no `BEFORE DELETE` guard on the document
    families is needed. T3b carries the test that keeps (i) honest: delete a
    discarded cancelled document and assert the ledger holds no row for it.

## Scope

In scope:

- Migration 42: `party_ledger_entries` + indexes; a Rust backfill run once right
  after `sqlx::migrate!` (see decision 9).
- `PartyLedgerRepository` (trait + `Sqlite…` impl in one file, `_in` twins).
- Entry writes in all four `confirm` paths, `record_payment` (sales/purchases),
  `pay_supplier`, `customer_receipts::collect`, and all four `cancel` paths.
- Transaction units for the payment/cancel paths that lack one.
- All party-balance reads moved to the ledger: customer balance, statement, ageing,
  credit-limit check, supplier drawer, payables.
- Explicit credit application at collection, plus credit-balance display (ES + EN).
- Tests pinning every behaviour change, including the refusals that are lifted.

Out of scope (follow-ups, recorded here so they are not silently invented later):

- Withdrawing a positive credit as cash (a customer asking for their 50 back).
  Credit applies to new documents only in v1.
- Any dashboard/KPI change — none sums party balances today (verified NOT FOUND).
- Party credit limits for suppliers (the flag exists only for customers).
- Changing `MovementReason`, price ladders, or any stock behaviour.
- The legacy upgrade path of migration 43 is **recorded, not fixed**: its
  `CREATE UNIQUE INDEX` runs against whatever `party_ledger_entries` already
  holds, and the backfill's empty-table guard is a TOCTOU (`count()` runs before
  `pool.begin()`, `src/repositories/party_ledger_repo.rs:1201-1209`), so two
  processes starting together on one legacy file could both backfill. No
  insert path in the current backfill can emit a duplicate
  `(document_kind, document_id, kind)` for an indexed kind (verified: four
  single-parent loops, no joins, disjoint `document_kind` families), so this is a
  narrow hazard and not a live defect. If it ever fires, the symptom is an
  aborted startup on a UNIQUE violation with manual remediation.

## Constraints

- Binary-only crate: every test is an inline `#[cfg(test)] mod tests` next to the
  code it covers; there is no `tests/` directory.
- No bare `cargo fmt`; `cargo check --all-targets` carries what AGENTS.md calls a
  75-warning baseline — **measured on this tree the numbers are 49 (test target)
  / 79 (bin target) / 93 by `grep -c warning`, so compare deltas, not absolutes** —
  and `cargo test --locked` is the CI gate together with `scripts/e2e.sh`.
- Never edit a shipped migration; append a new numbered file.
- Money is `Decimal` stored as `TEXT`; every new sum goes through
  `checked_money_add` / `checked_money_sum` / `checked_aggregate_sum` — a bare `+`
  on money is a defect.
- A new UI string means a `MessageKey` variant plus both ES and EN catalogs.
- A JSON API handler returns a named type, never `Json<Value>`.

## Tasks

- [x] **T1 — Migration 42 (table + indexes) + Rust backfill +
  `PartyLedgerRepository` + the fold.** Done in `0dfe5c1`; checks observed in
  Progress.
  Table `(id, party_type, party_id, kind, amount, document_kind, document_id,
  entry_date, reference, created_by, updated_by, created_at, updated_at)`, indexes
  on `(party_type, party_id, id)` and `(document_kind, document_id)`. Backfill is a
  Rust function called from `db.rs` after `sqlx::migrate!` (decision 9: document
  totals are derived and SQLite would do the arithmetic in REAL), guarded by an
  empty ledger table, one transaction, seeded from confirmed `sales` /
  `sale_payments` / `customer_returns` / `customer_return_payments` and the
  purchase mirror. Trait with `_in` twins; `balance_for_party` as a checked fold.
  Tests: fold signs per entry kind; `_in` join proven by rollback under
  `max_connections(1)`; backfill run directly against a seeded pool gives the old
  fold ± the returns correction; re-running on a non-empty ledger is a no-op.
- [ ] **T1b — Migration 43: append-only enforcement + the single-instance guard.**
  Decisions 10 and 11: `BEFORE UPDATE` / `BEFORE DELETE` triggers that abort, the
  partial unique index over the single-instance kinds, and `ALTER TABLE … DROP
  COLUMN updated_by, updated_at` (order matters: the columns must be dropped
  before the triggers exist, since SQLite refuses to drop a column a trigger
  names). Model and repository lose the two fields and every reference to them.
  Migration 42 is not touched. Tests first (RED), in the `role_repo.rs` style —
  the raw statement, `unwrap_err()`, and the database's own message:
  a second `Charge` for one document is refused; so is a second `Cancel`; two
  `Payment`s on one document are allowed; the index does not reach across
  documents; `UPDATE` and `DELETE` abort and leave the row and the fold intact.
- [ ] **T2 — Entry writes in the four `confirm` paths + lift the return caps.**
  `sales.rs`, `purchases.rs`, `customer_return.rs`, `purchase_return.rs`, inside the
  existing units via `_in`. Remove the two `return is worth … collected …` refusals.
  Tests first (RED): confirmed credit sale → `+total`; cash sale → `+total −total`;
  credit note → `−total` with no cash; return on a fully unpaid parent now confirms
  and reduces the balance; the two lifted-refusal tests are rewritten to assert the
  new outcome.
- [ ] **T3a — Transaction units + entry writes for the collection paths.**
  `record_payment` (sales), `record_payment` (purchases), `pay_supplier`,
  `customer_receipts::collect`. One unit each; cash transaction + payment row +
  ledger entry commit or roll back together. Lift the four overpayment refusals.
  Tests first (RED): partial failure leaves no payment row and no entry; paying 250
  against a 200 debt yields `−50`; receipt collection applies explicit credit then
  collects the remainder.
- [ ] **T3b — Transaction units + entry writes for the four cancel paths.**
  `sales::cancel`, `purchases::cancel`, `customer_return::cancel`,
  `purchase_return::cancel`: `cancel` + `refund` entries inside one unit, replacing
  today's autocommit sequence, **but only for a document that was confirmed at
  least once** (decision 12): a document cancelled while still a Draft appends
  nothing, because it never carried a `charge`. Tests first (RED): a cancelled cash
  sale lands on 0; a partial failure leaves the document Confirmed with no refund
  row; a discarded cancelled document (the deletable one, `sale_number IS NULL`) is
  deleted with no ledger row ever written for it.
- [ ] **T4 — Reads move to the ledger.**
  `customer_balance`, `ageing_of`/`customer_ageing`/`ageing_all`, the
  `ENFORCE_CREDIT_LIMIT` projection, `suppliers_web.rs:428` drawer fold,
  `outstanding_payables`, `customer_statement`. One `balance_for_party`. Buckets are
  the exact **per-document residual** (`charge` + `payments` of that document), with
  FIFO used only for the net excess that has no document left to land on, and a
  return attributed to its parent through the parent link the return family owns
  (decision 7, revised). Tests: a returned credit sale lowers balance, ageing and
  frees credit limit; supplier drawer reflects a confirmed return; a payment applied
  to the oldest invoice ages on the oldest invoice and is not moved by a later
  credit; the independent folds are deleted, not left dormant.
- [ ] **T5 — Explicit credit application in the collection UI.**
  Show available credit on the collection screen, explicit apply action, collect the
  remainder. `MessageKey` variant + ES + EN. Playwright coverage; regenerate the
  visual baseline only with proof that the diff is exactly what was intended.
- [ ] **T6 — Verification pass.**
  `cargo test --locked`, `cargo check --all-targets` (compare against the 75-warning
  baseline and explain any difference), `git diff --check`, `scripts/e2e.sh`.

## Route declaration

| Task | Route | Trigger evidence |
|---|---|---|
| T1 | delegated writer | migration + new repo file + tests = 2+ non-trivial files |
| T1b | delegated writer | migration + model + repository + tests = 2+ non-trivial files |
| T2 | delegated writer | 4 service files + tests |
| T3a | delegated writer | 4 service paths + lifted refusals + tests |
| T3b | delegated writer | 4 cancel paths + tests |
| T4 | delegated writer | 6 read sites across 4 files |
| T5 | delegated writer | route + template + JS + localization + e2e |
| T6 | fresh verification worker | full suite + browser suite |

Parent does: git state, feature-doc updates, per-task spot checks, delivery decisions.

## Acceptance criteria

- A confirmed purchase return reduces the supplier balance by the returned total,
  even when the parent collected nothing (the case refused today).
- A confirmed credit note reduces the customer balance, its ageing buckets, and
  frees the credit limit consumed by that sale.
- Paying more than owed leaves a negative balance — a displayed saldo a favor — on
  both a customer and a supplier, and does not error.
- Applying explicit credit at collection makes the operator collect only the
  remainder, and the ledger shows both the application and the cash.
- Every party-balance surface reads one function; no second fold over
  `due` survives in the tree.
- Every payment, collection and cancel is atomic: a failure leaves neither a
  payment row nor an entry nor a cash transaction.
- `cargo test --locked` and `scripts/e2e.sh` pass; warning count explained.

## Delivery

- Forecast at creation: **~1,800 authored lines** (migration, repository, six write
  paths, six read sites, UI, tests).
- Strategy: **Feature Branch Chain with tracker** (decided 2026-10-03; cached, not
  to be changed). One draft/no-merge tracker PR on `feat/party-ledger`, children
  stacked: child #1 targets the tracker branch, each later child targets the
  immediate parent branch. Main only ever sees the integrated feature.
- Slice map (one honest slicing pass, at task boundaries):

  | PR | Task | Depends on | Forecast |
  |---|---|---|---|
  | 1 | T1 migration + repository + fold | — | ~350 |
  | 2 | T2 confirm writes + lifted return caps | PR1 | ~300 |
  | 3 | T3a collection paths + lifted overpay refusals | PR2 | ~350 |
  | 4 | T3b cancel paths | PR3 | ~300 |
  | 5 | T4 reads move to the ledger | PR4 | ~400 |
  | 6 | T5 explicit credit application UI | PR5 | ~300 |

  Each PR body carries the dependency diagram with 📍 on the current PR, its
  start/end, prior dependencies and out-of-scope items; tests and docs travel with
  the unit they verify. If a slice exceeds 400 after honest slicing, it is reported
  with a `size:exception` recommendation rather than compressed.
- Push, PR creation and merge stay the user's decisions; local work-unit commits on
  `feat/party-ledger` are part of this authorized implementation.

## Progress

- 2026-10-03 — document created after a two-pass code map (CodeGraph + read-only
  explorer) and one product decision from the user (explicit credit application).
- 2026-10-03 — **T1 delivered and committed**: `0dfe5c1 feat(ledger): add party
  ledger schema, repository and backfill (T1)` on `feat/party-ledger` (6 files,
  1958 insertions). Verification observed: `cargo test --locked` **1493 passed /
  0 failed**; `cargo test --locked party_ledger` 7 passed; `cargo test --locked
  schema` 7 passed; `git diff --check` clean; rustfmt clean on the four touched
  files. Warnings 93 → 97 (+4, all in the deliberately unwired layer — the shape
  AGENTS.md predicts until the layer above calls it). **Warning-baseline
  discrepancy**: AGENTS.md documents 75, but that number matches no measurement
  tried on this tree (49 for the test target, 79 for the bin target, 93 by
  `grep -c warning` before the change) — the delta is the usable signal, not the
  absolute. Test-first was honest: RED was observed as a compile failure and as
  7 failing tests against a removed migration; GREEN was 7/7.
- 2026-10-03 — **T1 review BLOCKED, state preserved (session end).** Consent was
  granted by the user (`granted`, medium risk, one lens `review-reliability`);
  START created lineage `review-947422ad2ac0098e`, frozen target
  `sha256:398edfad5181c90a6acd5d15c7704ab7e6f52b373bdceb934239b645f3a8b8db`,
  revision `sha256:c58d555532beb5bf8a085586d187e7f293c0d99d8973b9b50c495d468175f4ad`,
  correction budget 200. The reviewer Task was refused **three times** by the
  transport with `opencode_review_transport_relay_refused (reason:
  provider_failed)` — a model-provider failure, not a Gentle AI defect, so there
  is no report to file. Between attempts the exact-lineage STATUS was re-queried
  each time and kept returning `action: collect` with the same bound slot
  (subject `sha256:1bcc2df083746cd1bb436928830a271007c6fba0a0d593be78f3fb6aee117b99`,
  order 0). Nothing was lost; no acknowledgement was attempted; no receipt exists.
- 2026-10-03 — **T1b delivered and NOT committed** (migration 43, decisions 10
  and 11): the append-only triggers, the partial single-instance index and the
  `updated_by`/`updated_at` drop. Files: `migrations/20240101000043_party_ledger_integrity.sql`
  (new, 50 lines), `src/models.rs` (+5/−6), `src/repositories/party_ledger_repo.rs`
  (+177/−16) — migration 42, `src/db.rs`, `src/repositories/mod.rs` and
  `src/main.rs` byte-identical (verified with `git diff --quiet HEAD`). Test-first
  was honest: RED was observed as four failing refusal tests where `unwrap_err()`
  received `Ok` (a duplicate `Charge` and a duplicate `Cancel` both committed; an
  `UPDATE` stored `999`; a mass `DELETE` removed both rows), GREEN was 13/13 after
  the migration plus `touch src/main.rs` — the embedded migration set is not
  re-read when a file is added. Full suite **1499 passed / 0 failed**. Warnings
  **delta 0** against the measured per-target baseline (79 bin / 49 test); the
  `93 → 97 (+4)` line above comes from a different grep shape and is not
  comparable to per-target counts, so the reproducible signal is the zero delta.
  Refusal texts observed verbatim: `UNIQUE constraint failed:
  party_ledger_entries.document_kind, party_ledger_entries.document_id,
  party_ledger_entries.kind` (SQLite names COLUMNS, never the partial index — a
  test asserting the index name could never pass), and the two trigger messages.
  An independent `gentle-ai-verify` run confirmed every claim, the Rust-column
  lists against the post-migration schema, and that no backfill path can emit a
  duplicated indexed kind.
- 2026-10-03 — **the three remaining plan-review gaps are now recorded, not implicit.**
  (c) the missing document-delete guard: closed as decision 12 with the user's own
  argument — a Draft writes nothing, so nothing can point at it, and `delete_draft`
  already refuses every non-Draft, so **no new trigger** is added; the requirement
  lands on T3b instead (a document cancelled from Draft appends no entries, with a
  test for the deletable case). (d) ageing: decision 7 **revised** by the user's
  choice — exact per-document residual with FIFO only for the net excess, plus the
  honest caveat that a `Return` entry names its own document and the parent link is
  read from the return family. (e) the ceiling is written into Objective: a party
  **subledger**, not a general ledger, with no verifiable double-entry invariant.

## Resume here

0. **Four paths are intentionally UNCOMMITTED** after `0dfe5c1`:
   `odd/tasks/party-ledger.md` (`M`), `src/models.rs` (`M`),
   `src/repositories/party_ledger_repo.rs` (`M`) and the untracked
   `migrations/20240101000043_party_ledger_integrity.sql`. The T1 review
   candidate is the commit `0dfe5c1` under `--committed-only`, so its bytes stay
   pinned and the drift does not touch it; leaving T1b uncommitted only keeps
   `feat/party-ledger` at the reviewed boundary. Open user decision: commit T1b
   (`fix(ledger): enforce append-only and single-instance kinds (T1b)`) plus the
   doc with a `docs(odd):` message, or wait for the review to resolve. Note that
   the blocked review is reviewing a schema state that migration 43 supersedes —
   migration 42's `updated_*` columns and the missing triggers no longer describe
   the tree.
1. Re-run the bound STATUS (read-only) and check it still says `collect`:

```
gentle-ai review status --contract=gentle-ai.review-integration/v2 --next-transition=true --lineage=review-947422ad2ac0098e --repository-context=rctx2_f0c2737a5f1e905298e6156900b1918ba3fcb2a163108a402e3f2163a188ab88 --agent=opencode --base-ref=86c69adbb1eef4d569b0a2fe4d8412d91a16d38e --committed-only=true
```

2. If it returns `collect`, relaunch the lens **once**: one OpenCode Task with
   `agent` copied exactly as `review-reliability` and `prompt` copied
   byte-for-byte from `next_transition.collect.inputs[0].provider_task.prompt`
   (451 chars, starts `GENTLE_AI_REVIEW_BINDING {…}`). Never rebuild the binding
   from prose or from the arguments list.
3. If the capture is admitted, follow the returned transitions to
   acknowledgement (the exact acknowledgement burns the authority). If the
   provider still refuses, the lineage stays open — decide then between waiting
   for the provider and `gentle-ai review mode disable --scope clone`
   (a user-owned switch; only the user may choose it).
4. The reviewed boundary only advances once `0dfe5c1` is acknowledged; until
   then the next assess still runs with `--base-ref main --committed-only`.
5. After the commit decision: next implementation task is **T2** — entry writes
   in the four `confirm` paths plus the two lifted return-cap refusals, tests
   first. All five gaps the plan review found (Engram
   `review/party-ledger-design-gaps`) are now closed or recorded: (a) and (b) in
   migration 43, (c) as decision 12, (d) in the revised decision 7, (e) as the
   Objective paragraph. Nothing about them is left to be remembered during T2.
