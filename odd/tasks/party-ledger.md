# Party ledger — cuenta corriente por tercero

## Status

Planned. No migration, no source file, no template touched yet. Nothing in the tree
has been changed by this document.

## Objective

One ledger of signed entries per party (customer, supplier) that becomes the single
source of truth for every party balance in the app: outstanding debt, saldo a favor
(credit), ageing, credit limit, payables, and the account statement.

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

7. **Decided 2026-10-03 — reads come from one place.** `balance_for_party` (checked
   fold over entries) replaces `customer_balance`, the supplier drawer fold, and
   `outstanding_payables`. Ageing allocates the net balance across confirmed credit
   documents oldest-first (FIFO), so a credit balance lands on the oldest open
   document for bucket purposes while the total stays one number.

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

## Constraints

- Binary-only crate: every test is an inline `#[cfg(test)] mod tests` next to the
  code it covers; there is no `tests/` directory.
- No bare `cargo fmt`; `cargo check --all-targets` carries a 75-warning baseline and
  `cargo test --locked` is the CI gate together with `scripts/e2e.sh`.
- Never edit a shipped migration; append a new numbered file.
- Money is `Decimal` stored as `TEXT`; every new sum goes through
  `checked_money_add` / `checked_money_sum` / `checked_aggregate_sum` — a bare `+`
  on money is a defect.
- A new UI string means a `MessageKey` variant plus both ES and EN catalogs.
- A JSON API handler returns a named type, never `Json<Value>`.

## Tasks

- [ ] **T1 — Migration 42 (table + indexes) + Rust backfill +
  `PartyLedgerRepository` + the fold.**
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
  today's autocommit sequence. Tests first (RED): a cancelled cash sale lands on 0;
  a partial failure leaves the document Confirmed with no refund row.
- [ ] **T4 — Reads move to the ledger.**
  `customer_balance`, `ageing_of`/`customer_ageing`/`ageing_all`, the
  `ENFORCE_CREDIT_LIMIT` projection, `suppliers_web.rs:428` drawer fold,
  `outstanding_payables`, `customer_statement`. One `balance_for_party`, FIFO
  allocation for buckets. Tests: a returned credit sale lowers balance, ageing and
  frees credit limit; supplier drawer reflects a confirmed return; the independent
  folds are deleted, not left dormant.
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
