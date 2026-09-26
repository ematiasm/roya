# Tax contract overflow — make the shared tax arithmetic total

## Objective

Make the shared tax arithmetic contract refuse instead of panicking, and bound the
inputs that reach it, so that no authenticated operator can drop a request by
typing a large quantity, a large price, or by an admin having stored an extreme tax
rate.

## Problem

`calculate_line_taxes` performs its arithmetic with rust_decimal's raw operators,
which **panic** on overflow rather than returning an error. Five production call
sites hand it user-controlled or user-adjacent amounts with no upper bound.

The confirmed failure, reproducible on today's code with a tax an admin can legally
create in Settings:

1. An admin creates a tax with an extreme rate — `validate_rate` enforces only
   `rate >= 0`, so `Decimal::MAX` is storable. The Settings input is
   `type="text" inputmode="decimal"` with no `max`, the JSON API has no attribute,
   and the column is `rate TEXT NOT NULL` with no CHECK.
2. A product is saved with a large `sale_price`. `validate_effective_prices` checks
   `> 0` for a product and has no upper bound **by design**.
3. Opening that product's drawer calls the price ladder, which calls
   `calculate_line_taxes(ladder.net_price, &taxes)` on the stored net.
4. `net * rate` overflows, the handler task panics, the connection is dropped.

Step 3 is an **ordinary page load**. No crafted request is required. A draft sale or
purchase line reaches the same contract through `qty * unit_price` with both
operands unbounded.

There is no `catch_unwind` and no panic-catching layer anywhere in `src/`, and
`Cargo.toml` has no `[profile]` section, so the default unwind profile applies. The
panic escapes the handler, axum's per-connection task resolves to a dropped
`JoinHandle`, and the operator gets **no response at all** — not a 500, not a
message in any language. On the HTMX-driven screens the drawer silently fails to
refresh and the operator's typed values appear to have vanished.

## Why now

This was found while verifying the final-price solve
(`odd/tasks/final-price-markup.md`, U1). That module bounds its own input and
refuses; its own doc names the five unguarded sites and states explicitly that it
does **not** make the tax contract total. A green suite in one module must not read
as evidence that the shared contract is safe. The user chose to fix this as its own
feature now, before the final-price work wires a route.

## Evidence

Confirmed by read-only investigation with file:line evidence. Facts, not inferences.

| Fact | Evidence |
| --- | --- |
| Raw `Mul` panics | `src/services/line_taxes.rs:151` — `net_subtotal * tax.rate / percent()` |
| Raw `Add` panics, and it is a **second, independent** site | `line_taxes.rs:169` — `net_subtotal + tax_total` |
| A multiply-only fix still panics | probe: every individual multiply fits, the running `tax_total` sum fits, only the final add overflows |
| `tax_total += amount` at `:153` is dominated by `:169` | for a non-negative rate set, `tax_total > MAX` implies `net + tax_total > MAX` |
| 5 production call sites, all HTTP-reachable | `sale_repo.rs:343`, `sale_repo.rs:398`, `purchase_repo.rs:313`, `purchase_repo.rs:366`, `taxes.rs:394` |
| 2 further call sites, no production reader | `final_price.rs:369`, `final_price.rs:477` — the module's only caller is its own test helper |
| The amount argument is itself a raw `*` | all four repository sites evaluate `qty * unit_price` or `qty * unit_cost` before entering the contract |
| `validate_rate` has no ceiling | `taxes.rs:455-460` — five lines, one rule, `rate >= 0` |
| `add_line` has no ceiling | `sales.rs:353,360`; `purchases.rs:376,417` — `qty > 0`, `unit_price >= 0` |
| Read-side raw multiplies exist | `SaleLine::subtotal` `models.rs:1079`, `PurchaseLine::subtotal` `models.rs:1574`, feeding `tax_split` `sales.rs:200` / `purchases.rs:183` and every detail total |
| One raw `Add` outside the contract | `purchases.rs:487` — `existing.qty + qty` in the scan-merge path |
| No panic containment | zero matches for `catch_unwind`, `AssertUnwindSafe`, `CatchPanicLayer` in `src/` |
| Nothing pins the current behaviour | zero `#[should_panic]` in the repository; no test asserts a panic |
| An existing typed-error convention for this exact defect class | `PriceRefusal::DerivationOverflow` `models.rs:238`, produced by `checked_mul` at `inventory.rs:76-83`, with the same stated reason |
| A rate ceiling breaks nothing | zero existing tests store a rate above 100; the only service-level rate test asserts the negative refusal |
| `Decimal` limits | `rust_decimal 1.43.0`, `legacy-ops` **not** enabled. `MAX_SCALE = 28`, `MAX = 79228162514264337593543950335` (≈7.92e28) |

## Scope

- Make `calculate_line_taxes` total: a checked result instead of a panic.
- Cover the five production call sites, including the raw `qty * unit_price`
  argument they compute.
- Add an upper bound to `validate_rate`.
- Pin the invariant that makes the read path safe.

## Out of scope

- A stored-data migration or audit of existing production rows. Whether the real
  deployment already holds an extreme rate or price is **unverified** — the database
  was deliberately not opened. That is its own piece of work.
- Catching panics globally. A `CatchPanicLayer` would convert a defect into a
  generic 500 and would hide the next one. This feature removes the cause.
- `purchases.rs:487` `existing.qty + qty` in the scan-merge path. Confirmed as a
  code path, not confirmed as reachable with values that overflow. Recorded as a
  follow-up, not fixed here.
- Verifying whether a panic inside the open transaction at `sale_repo.rs:336` leaves
  the SQLite connection usable. `sqlx::Transaction`'s drop should roll back during
  unwinding, but that was not verified against sqlx 0.9. Treat "no partial write" as
  unverified.
- Any change to the rounding contract. `round_to_cents` stays the only money
  rounding rule.

## Decisions

1. **The contract returns a typed error; it does not get a helper.** Changing
   `calculate_line_taxes` to return a `Result` makes totality structural: a future
   caller cannot forget the guard, because the signature forces them to handle it. A
   checked-multiply helper would leave every existing raw `+` in place. Blast radius
   is 7 non-test call sites across 5 files, plus one test helper.
2. **The typed error reuses the existing refusal architecture.** `PriceRefusal` +
   `AppError::PriceRefused` + `price_refusal_key` + both catalogs, following
   `DerivationOverflow`. No parallel error type.
3. **Two distinct arithmetic refusals, not one.** The line amount itself
   unrepresentable, and a representable amount whose tax arithmetic is not, are two
   different operator remedies. They must not share a variant.
4. **The rate ceiling lives in `validate_rate`, using the existing tax validation
   convention** (`AppError::Validation`), not `PriceRefusal`. A rate is a tax
   definition, not a price refusal; putting it in `PriceRefusal` would blur the
   domain that enum was created to keep precise.
5. **The rate ceiling value is 1000%**, held as a named constant. This is a
   business-plausibility bound, not an arithmetic one: with the ceiling in place the
   worst safe net is still ≈7.2e27, so the ceiling cannot cause a false refusal, and
   its real job is to turn `Decimal::MAX` into a form error in Settings rather than an
   arithmetic refusal three layers down. If the business ever needs a higher levy the
   constant and its argument move together.
6. **The governing bound is `net × (1 + Σ Rᵢ/100) ≤ MAX`**, not the per-multiply
   `net × Rᵢ ≤ MAX`. For a non-negative rate set the pair is tighter, and the probe
   shows the final add is the site that actually fails.
7. **The read path is closed by the write bound, not by its own guards.** If a write
   refuses any amount that cannot be represented, then `SaleLine::subtotal` and
   `PurchaseLine::subtotal` can only ever multiply values that already passed that
   bound. This is a real invariant and it is exactly the kind of load-bearing
   assumption that rots silently, so it gets a test that pins it rather than a
   comment that states it.
8. **A refusal is not a crash.** Every refusal must reach the operator in their
   language through the existing single renderer, on the same screens that today
   return nothing at all.

## Constraints

- Strict TDD. Observe RED before implementation, then GREEN, then a refactor pass.
- `round_to_cents` remains the only money rounding rule. No second one.
- `calculate_line_taxes` remains the only definition of the final price. No second
  formula.
- No second error-mapping table, no second localization route. The closed-set
  totality tests must keep passing.
- Technical artifacts, code, comments, and UI copy in English.
- No push, PR, or merge without explicit user authorisation.
- `odd/tasks/pos-counter-sales.md` is unrelated untracked work: never stage it.
- Test pools are `sqlite::memory:`. Never open the development `roya.db`.

## Work units

### T1 — the contract becomes total, and the five sites are covered

- `calculate_line_taxes` returns a typed error; every arithmetic step inside it is
  checked, including the final add that the probe isolated.
- Both raw `qty * unit_price` / `qty * unit_cost` arguments become checked.
- The five production call sites propagate the refusal instead of panicking.
- Two new refusal variants with EN and ES catalog rows, wired through
  `price_refusal_key`.

RED first, and it must be behavioural: a real overflow that panics today becomes a
refusal, on a sale draft line, a purchase draft line, and the product drawer, plus a
test that proves the operator receives a message rather than a dropped connection.

### T2 — rate ceiling, and the write-bound invariant is pinned

- `validate_rate` gains the upper bound with its own refusal and message.
- A test pins that a stored amount can never be unrepresentable, so the read-side
  multiplies are safe by that invariant. Say in the test what it is protecting.

RED first: a rate above the ceiling is refused in both languages, and a rate at the
ceiling is still accepted so the boundary is a bound and not a blanket rejection.

## Acceptance criteria

- [ ] No code path reachable from an HTTP request can panic on tax arithmetic.
- [ ] `calculate_line_taxes` cannot panic, and its totality is proven by a test, not
      asserted in a comment.
- [ ] Every refusal reaches the operator as a localized message, on the sale, purchase
      and product paths.
- [ ] A rate above the ceiling is refused; a rate at the ceiling is accepted.
- [ ] The closed-set refusal and localization tests still pass with no duplicate and
      no missing row.
- [ ] No new money rounding rule and no second final-price formula.
- [ ] No test panics anywhere in the suite.

## Verification

```bash
cargo test line_taxes
cargo test tax_snapshot
cargo test tax
cargo test price_refusal
cargo test translation_catalogs
cargo test sale
cargo test purchase
cargo test product
cargo test
cargo check --all-targets
cargo fmt --check
git diff --check
bash scripts/e2e.sh        # T1: sale, purchase and product paths are user-visible
```

Warnings are reported per profile against the `main` baseline. An e2e or visual
check is **not** N/A for either unit here: both change what an operator sees.

## Risks and open questions

- **`purchases.rs:487` `existing.qty + qty` is out of scope but unverified.** If
  the scan-merge path can accumulate a quantity that overflows when added, that is
  a third defect from the same family. Not fixed, not disproved.
- **The Evidence table is NOT a complete census, and this is a known gap in it.**
  Independent verification of T1 found `src/services/purchases.rs:1349`,
  `subtotal: suggested_qty * unit_cost` — a raw `*` on the
  `GET /api/purchases/suggestions` path, with neither operand bounded
  (`record_cost` validates only `cost >= 0`; `suggestion_for` returns
  `max - stock` from operator-set stock levels with no ceiling). An operator stores
  an extreme supplier cost and the request is dropped on an ordinary page load.
  Neither this nor `purchases.rs:487` is *tax* arithmetic, so the T1 acceptance
  criterion holds as written; both are the same family and both are unfixed.
- **A rate ceiling changes what is storable.** An existing deployment with a stored
  rate above the ceiling would still load, but could no longer be updated through
  Settings. Whether that is acceptable is a data question, and the data was not read.
- **The read-path invariant is a single point of failure.** If a future write path or
  a migration inserts an unrepresentable amount directly, the read multiplies panic.
  T2's test is the guard against that; it must assert the invariant, not just the
  absence of a bug today.
- **Whether a panic inside the open transaction leaves the connection usable is
  unverified.** This feature removes the trigger, not the uncertainty.

## Progress

- Evidence gathered by read-only investigation; the census, the blast radius, the
  limits, and the two raw-operator sites are confirmed above.
- No implementation yet.

### T1 — contract total + five sites

Not started.

### T2 — rate ceiling + write-bound invariant

Not started.
