# AGENTS.md

Roya — local business manager. Rust 2021 + Axum 0.8 + SQLx/SQLite + Askama + HTMX + Tailwind 4.
Read this before touching `src/`. Most of what follows is a rule that a future agent
cannot infer from the code.

## Commands

```bash
cargo test --locked            # what CI runs. 1486 test attributes under src/ (2026-10-02)
cargo test <filter>            # test names are long descriptive sentences, so filter on a phrase
cargo check --all-targets      # 0 errors; 75 warnings is the accepted baseline
git diff --check               # whitespace
scripts/e2e.sh                 # browser suite. uv + Playwright, NO Node toolchain
scripts/e2e.sh -k scan         # args pass straight through to pytest
scripts/build-css.sh           # regenerates static/tailwind.css
```

- **CI gates on exactly two things:** `cargo test --locked` and `scripts/e2e.sh`
  (`.github/workflows/checks.yml`). `fmt` and `clippy` are deliberately NOT gates — the
  workflow comment at `checks.yml:5-10` says making them gate would be "a new rule
  arriving through a side door". CodeQL (`.github/workflows/codeql.yml`) is a separate
  upload-only scan on `main`, not a PR gate.
- **Do not run bare `cargo fmt`.** The working tree carries deliberately
  one-file-per-commit refactor slices; a repo-wide reformat buries them in noise.
  Use `cargo fmt --check` to see whether *your* files drift.
- **The 75 warnings are the baseline, not debt to clear.** A PR that drops them
  opportunistically is scope creep. The count is a snapshot; the rule is not — compare
  against it and explain a difference, don't assume it.
- **Dead code is transitive, and this crate cannot opt out of it.** There is no
  `src/lib.rs`, so `pub mod` and `pub use` do not make a module live: analysis runs
  from `main`, and a symbol used only by code nothing reaches stays dead. So a new
  layer does not lower the count — it raises it until the layer *above* calls it.
  A count that finally drops is the evidence the wiring is real. That is a
  measurement worth making, not a formality.
- `static/tailwind.css` is **generated but committed** so `cargo run` works without the
  Tailwind CLI. Touching `assets/tailwind.css` means re-running `scripts/build-css.sh`
  and committing both.
- Migrations run automatically at startup via `sqlx::migrate!("./migrations")`
  (`src/db.rs`). Never edit a shipped migration; append a new numbered one.

## Shape

- **Binary-only crate.** There is no `src/lib.rs` — everything is a module declared in
  `src/main.rs:1-24`, tests included. You cannot integration-test internals or
  `use` anything from a `tests/` dir. There is no `tests/` dir at all: every test is an
  inline `#[cfg(test)] mod tests` next to the code it covers.
- Layering is `routes → services → repositories`. A service is generic over repository
  traits and **never holds a `&SqlitePool`**; a repository holds only `pool: SqlitePool`.
  Concrete aliases (`SalesSvc`, `InventorySvc`, …) live in `src/routes/mod.rs:60-177`,
  not in `services`.
- Each repository file is trait + impl, in that order, in the same file: `#[async_trait]
  pub trait X`, then the `SqliteXRepository` struct, then `impl X for SqliteXRepository`.
- **A JSON API handler returns a named type, never `Json<serde_json::Value>`.** The wire
  contract is a compile-time concern here: a renamed key in a `Value` compiles, the
  suite still passes, and only an external client breaks. Response types live beside the
  type they wrap — an envelope whose payload is defined in `models.rs` goes there, and
  `CustomersResponse` / `AgeingResponse` stay in `customers_api.rs` because
  `CustomerBalanceView` and `CustomerAgeingView` do. `SetMoney` fields stay `SetMoney`:
  its `Serialize` is what publishes the refusal rule instead of a bare `null`.
- **A JS consumer with a fallback turns a wire break into a working-looking empty
  screen.** The picker island reads `data.products || []` (`static/picker.js`), so a
  renamed key does not raise, does not log, and sets its status to `done` — it renders
  an empty dropdown on the ten templates that mount it. `ProductSearchResponse` is
  named for exactly that reason. Any `||` or `??` default on a response field hides a
  contract break the same way; prefer a guard that says the key is missing.
- Some `src/services/` modules are **pure** — `line_taxes`, `gross_inverse`,
  `final_price`, `purchase_cost`. Each holds the single definition of a rule. Do not
  re-derive a price or a tax split inline anywhere else; call the function.
- Errors: one `AppError` (`src/error.rs:11`), nine variants, `AppResult<T>`. Only
  `Database` uses `#[from]`; every other raise is explicit `AppError::X(..)` at the site.
  `PriceRefused(PriceRefusal)` carries a typed *rule*, never a message.
- Adding a UI string means adding a `MessageKey` variant (`src/localization/`) and both
  the ES and EN catalogs — the enum is closed by `define_message_keys!`. A count key
  whose singular differs from its plural must be listed in `count_keys` in
  `src/localization_tests.rs`; the test fails otherwise and the workaround is a
  grammatically wrong label, not a smaller list.
- **Three document families now mirror each other**, not two: sales, purchases, and
  sales' returns / purchases' returns (`purchase_returns` + `customer_returns`, three
  tables each). A change to one family's service, repository or templates is a template
  for the other two — read the sibling before inventing a third shape.
- A return line points at the **parent LINE**, never at a product. A second copy of the
  product could name something different than the line it claims to return and nothing
  in the schema would notice.

## The `_in` transaction convention

A repository method that must be able to **join a caller-owned transaction** has a
sibling suffixed `_in` that takes `&mut sqlx::SqliteConnection` as the first parameter
after `&self`. The public twin does nothing but open, delegate, commit:

```rust
let mut tx = self.pool.begin().await?;
let r = self.x_in(&mut tx, /* same args, minus tx */).await?;
tx.commit().await?;
Ok(r)
```

Rules that are not obvious from the signature:

- The param type is `&mut SqliteConnection`, **not** `&mut Transaction<'_, Sqlite>`.
  Callers pass `&mut tx`; query sites then write `&mut *tx`.
- One copy of the SQL, never two. Where a query runs on both executors, it is a free
  function generic over `E: sqlx::Executor` (`transaction_repo.rs:131`). Do not duplicate
  the statement per method.
- **Reads get an `_in` too**, not just writes. A pre-check that reads a level and then
  writes against it must see the caller's uncommitted rows
  (`balance_for_account_in`, `stock_for_product_in`).
- Grep for `_in` is noisy: `find_method_in_account` / `create_in_account` are an *infix*
  meaning "method **in** an account". Unrelated to this convention.
- The convention is stated in the trait docs themselves, which are the durable source:
  "Nothing opens a transaction yet. This is the door." The confirm path now walks
  through every one of these doors — `SalesService::confirm` and
  `PurchasesService::confirm` open ONE unit and every call inside it is an `_in` form
  (`src/services/sales.rs:1402`, `src/services/purchases.rs:1167`). The return services
  added afterwards needed **no new seam at all**, which is the evidence the convention
  was built for this. Before adding a repository method, check whether its caller
  already holds a transaction — an unconditional `pool.begin()` in the middle of a
  caller's unit is the bug the `_in` exists to remove.

## Money and stock

- **`Decimal`'s raw `+`/`-` panic on overflow, and so does `Iterator::sum`** — the code
  calls it out by name (`src/services/mod.rs:44-46`). Every money fold goes through one
  of three guards:
  - `services::checked_money_add` / `checked_money_sum` → `PriceRefusal::DocumentTotalTooLarge`
    (sums built *from* documents)
  - `repositories::checked_aggregate_sum` → `PriceRefusal::AggregateTooLarge`
    (folds inside a repo over one account's transactions or one product's movements)

  The two refusals are distinct on purpose. A bounded write says nothing about the sum
  of a set of them; a bounded line says nothing about the document total. Any new `+` on
  money is a defect.
- `ORDER BY id` in the two level folds (`transaction_repo.rs:152`, `stock_repo.rs:268`)
  is **load-bearing**. The check runs on the *running* sum, so row order decides which
  prefixes are seen; the `(account_id, date)` index can otherwise return rows in date
  order and the two folds disagree about the answer, not just the prefixes. Do not
  "optimize" it away.
- `cached_balance` is a cache, never a read source. Balance is always the derived signed
  sum. It is refreshed inside the writing unit, so a rollback must take it back.
- Sign conventions are three separate functions and should not be unified casually:
  `repositories::signed_amount` (Income `+`), `stock_repo::signed_contribution`
  (In `+`, Out `−`, **`Adjust` passes `qty` straight through**). An `Adjust` is a signed
  delta — a negative one is a legitimate increase, so "negative means out" is false.
- The overdraft guard fires **only on `Expense`** (`src/services/transaction.rs:112`). A
  refund is `Income` and is never blocked, which is intentional
  (`src/services/purchases.rs:19-21`). Widening it to both directions is a behaviour
  change, not a bug fix.
- `ALLOW_NEGATIVE_BALANCE` defaults false, `ALLOW_NEGATIVE_STOCK` defaults **true**,
  `ENFORCE_CREDIT_LIMIT` defaults true (`src/main.rs:49-57`). They are **constructor
  flags** baked into the services at `src/routes/mod.rs:419-497`, not per-call arguments.
- **A service that shares another's dependencies must share its FLAGS too.** The return
  services take the *same* `inventory_service` and `transaction_service` instances the
  purchase and sale services hold. That is deliberate: a second instance with a different
  `ALLOW_NEGATIVE_STOCK` would make "can stock go negative" depend on WHICH document moved
  the goods — one fact read by two rules. `ENFORCE_CREDIT_LIMIT` is absent from the
  return constructors because no return service takes it; a flag nobody can act on is how
  a guard rots.
- Money is stored as SQLite `TEXT` and serialized as a **string**. Never `f32`/`f64`.
- `parse_decimal` silently maps a malformed stored value to `ZERO`. `markup_pct`
  deliberately uses the strict variant instead, because there `0%` is a meaningful value
  and malformed must degrade to `None`, not pin sale price to cost.
- `SetMoney::amount` is `None` **exactly when** `refusal` is `Some` — never return a
  partial `Decimal` with a flag. `amount_is_positive()` returns `true` for a refused
  figure; a refusal is a colour, not a sign.
- `MONEY_SCALE` is defined twice on purpose (`localization` is a leaf and cannot depend on
  `services`). A test pins them; do not "deduplicate" and break that.
- Timestamp columns compare lexically. `db::encode_sqlite_timestamp` is the only safe
  binder — chrono's `Display` (`2024-05-01 12:00:00`) sorts before every DB-written
  `2024-05-01T12:00:00.000Z` because `' ' < 'T'`.

## Sales vs purchases on duplicate product lines

This is the sharpest asymmetry in the codebase and it is not symmetric by accident.

- **Sales ALLOW the same product on two lines. Purchases FORBID it.** A purchase refuses
  outright (`src/services/purchases.rs:1015-1027`) because `product_supplier_costs` is
  `UNIQUE(product_id, supplier_id)`, so two lines for one product has no defined cost
  answer (`src/services/sales.rs:1333-1345`).
- The sales stock pre-check **must aggregate demand per product** before comparing to the
  level. A per-line check re-reads the same unmutated level each iteration, so `6 + 6`
  against a level of `10` passes twice, the document number is burned, the first movement
  commits, and the second is refused downstream — leaving a movement whose `reference`
  names no sale.
- The `demanded` fold must be *checked*, not `sum` — two free lines of `5e28` each are
  `1e29` while the document total stays `0` and never trips.
- The stock **write** phase legitimately iterates per line (each line is a real movement).
  That is why `stock_for_product_in` reads the level once per product, not per line.
- Test gotcha: the `svc()` helper builds with `allow_stock = true`, so the strict branch
  never runs. Strict-branch tests must use `svc_with_flags(false, false)` or they pass
  for the wrong reason.

## Tests

- `#[tokio::test]` for anything touching the DB or HTTP; plain `#[test]` for pure
  functions.
- Fixture pattern is uniform: `sqlite::memory:` + `.create_if_missing(true)
  .foreign_keys(true)` + `SqlitePoolOptions::max_connections(1)` + `sqlx::migrate!`.
- **`max_connections(1)` is load-bearing, not an accident** — the Phase A tests rely on
  it to prove an `_in` method never reaches for the pool. Raising it hides the bug.
- Test names are full descriptive sentences, not category prefixes
  (`create_in_joins_an_open_transaction_instead_of_opening_one_of_its_own`).
  One legacy `ac<N>_` acceptance-criteria family survives in `sales.rs`.
- There is **no test-only auth bypass**. `security::test_support::seed_session` creates a
  real session with a full-catalog role, so `Require<P>` is genuinely satisfied.
- Failure injection uses `sqlx::raw_sql` + `CREATE TRIGGER ... RAISE(ABORT, ..)`.

## `confirm` is atomic — a Draft IS clean

This is measured by tests, not inferred, and it is the reason `delete_draft` can admit a
Draft without checking anything else.

`SalesService::confirm` and `PurchasesService::confirm` open ONE transaction
immediately before taking the sequence number and commit after the last write. Every
call inside the unit is an `_in` form. A failure at any step rolls the whole run back:
no number, no movement, no finance row, no payment — and `doc_sequences` has **no row at
all**, so the ticket number is returned rather than burned, and a retry takes the first
number. Both families have a test per injected failure window using
`CREATE TRIGGER ... RAISE(ABORT)` on each write in turn, asserting the state afterwards
rather than inferring it.

**This was not always true, and the earlier residue tables are the reason the shape is
worth protecting.** Before this, the writes ran on separate autocommit connections with
`set_confirmed` LAST, so a failure at that last step left a row reading `("Draft", NULL)`
that had already committed a payment and an orphan `Income` naming the burned number.
Such a Draft matched the deletable predicate, and deleting it lost the document and kept
the money. If you ever see that shape described anywhere, it is describing the past.

Three rules still hold:

- **The cleanliness of a Draft is a property of `confirm`, not of the status.** The
  predicate on `delete_draft` is a backstop on status; the Draft behind it is clean
  because the transaction rolls back. Remove the transaction and this whole section is
  wrong again — which is why the reason is written at the call site rather than left to
  be inferred from the SQL.
- **Do not add a state gate to `create_payment`.** It has none deliberately. Four
  production callers reach it, and two of them collect against an already-**Confirmed**
  document — `record_payment_with_receipt` (`src/services/sales.rs:1502`) and
  `record_payment` (`src/services/purchases.rs:1266`). A `Draft` gate would refuse the
  collection of a confirmed credit sale.
- **A new confirm path inherits this for free only if every write inside it is an `_in`
  call.** The return services were added after this refactor and needed no new seam
  precisely because of that. Reaching for a public twin from inside a unit re-opens the
  class of bug above — and it fails as a `PoolTimedOut` deadlock, not as wrong data,
  so the mutation proof is worth running.

## Stale references

- The README deliberately carries **no** test commands, no file tree and no SQLite →
  Postgres migration guide. Those live here, or nowhere. The Postgres note is not an
  omission to undo: `sqlx` has the `sqlite` feature only, so it has never been run on
  Postgres, and two repository traits say "portable to Postgres" as a design property
  rather than a tested one.
- `.github/workflows/checks.yml` says "~900 tests". The real count is 1480 test
  attributes. Trust the code, not the comment.
- `odd/tasks/` holds one feature document per piece of work, and several are explicitly
  marked as superseding older notes on the same topic. Read the one that says
  "supersedes" before acting on any of them.

## Working on the interface

- **A feature that delivers a screen is not done until someone has looked at it.**
  1480 Rust tests and 134 browser tests all passed while purchase returns and credit
  notes were unreachable and uncreatable: the list pages returned 200 with no console
  errors and had no sidebar entry, and the creation dialog posted an empty id because
  nothing filled it. No suite catches a missing link or a dead button, because both are
  statements about what a person sees, not about what the code returns.
- To look: drive the real binary with Playwright from `e2e/` (`uv run`, no Node
  toolchain), screenshot, and read the PNG back. `e2e/helpers.py` has the seed helpers
  and `e2e/conftest.py` shows the spawn shape — `DATABASE_URL`, `PORT`, `RUST_LOG` into
  `target/debug/roya`.
- **The visual baseline (`e2e/visual-baseline.json`) is a real gate.** One new Tailwind
  class in a template fails the suite until `scripts/build-css.sh` regenerates it.
  Prefer a class the codebase already uses. If you must regenerate, prove the diff is
  only what you intended — the file is one minified line, so `git diff` is useless; load
  both versions as JSON and compare the page keys.
- **The record page's header does not refresh after an action.** `page_header.html` sits
  OUTSIDE the `id="…-record"` element every `htmx` action swaps, so a confirmed
  document still shows its Draft title and its confirm button until a manual reload.
  Pre-existing and house-wide across purchases, sales and returns.
- Askama has **no `and`** — `{% if a and b %}` is a parse error; nest them. Askama also
  cannot compare decimals, so every derived boolean has to move into Rust, and
  `Decimal::ZERO.is_sign_positive()` is `true`.
- `Form<T>` rejection is a `Response` the handler never sees, so `map_err` inside the
  handler cannot translate it. A field that cannot be read surfaces as serde internals
  unless the handler takes `Result<Form<T>, FormRejection>`. And `#[serde(default)]` on
  an `i64` turns a missing required id into `0`, which reads as "document not found".

## Memory and indexes

- A live **CodeGraph** index is present (`.codegraph/`). Prefer `codegraph_explore` over
  grep+read for structural questions — it returns verbatim source plus blast radius in
  one call. Run `codegraph status` first if results look stale.
- **Engram** project is `roya` (50+ observations). It holds hard-won corrections that are
  not in the code or the README — notably measured residue tables for the `confirm`
  failure windows and an Odoo 19.0 comparison of where the request-cursor transaction
  actually lives. Search it before re-deriving any of that.
