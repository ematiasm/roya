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
   business-plausibility bound, not an arithmetic one. With the ceiling in place the
   largest net a SINGLE tax at the ceiling can still compute against is
   `MAX / 1000 ≈ 7.92e25` — see Decision 6 for why the per-multiply bound and not
   the pair bound is the binding one, which is the correction that moved this figure
   from the `MAX / 11 ≈ 7.2e27` this document originally recorded. `7.92e25` is still
   not a number an operator types into a price box, so the ceiling cannot cause a
   false refusal, and its real job is to turn `Decimal::MAX` into a form error in
   Settings rather than an arithmetic refusal three layers down. If the business ever
   needs a higher levy the constant and its argument move together.
6. **The governing bound is the TIGHTER of two, both of which are checked** — the
   pair `net × (1 + Σ Rᵢ/100) ≤ MAX` (enforced by the checked running add and the
   checked final add) and the per-multiply `net × max(Rᵢ) ≤ MAX` (enforced by the
   checked multiply). **This corrects this document's earlier claim that the pair is
   always the tighter of the two. It is not.** The contract multiplies by the rate
   BEFORE dividing by 100 (`line_taxes.rs:207-210`), so for a single rate `R` the two
   bounds are `net ≤ MAX/(1 + R/100)` and `net ≤ MAX/R`, and the per-multiply binds
   as soon as `R > 1/(1 − 1/100) ≈ 1.0101%` — not above 100%, and not only for
   extreme rates. At `R = 1000` the per-multiply leaves `MAX/1000 ≈ 7.92e25` while
   the pair leaves `MAX/11 ≈ 7.2e27`, looser by `1000/11 ≈ 91×`. Anyone quoting a
   "largest safe net" must take the minimum of the two. The final add is still the
   site the probe isolated: an input exists where every multiply fits, the running sum
   fits, and only `net + tax_total` leaves the range.
7. **The read path is closed by the write bound, not by its own guards — PER LINE.**
   If a write refuses any amount that cannot be represented, then
   `SaleLine::subtotal` and `PurchaseLine::subtotal` can only ever multiply values
   that already passed that bound, and `tax_inclusive_total` can only ever add a
   `tax_total` that was already proven addable. That is a real invariant and it gets
   a test that pins it rather than a comment that states it. **It is not a
   document-level invariant, and this document's T2 test originally over-claimed that
   it was.** Per-line carryability does not carry a document SUM: see T3. A per-line
   census cannot pin a document invariant, because the property T3 needs is a
   statement about a SET of rows, and a census that only ever looks at one row at a
   time cannot see it.
8. **A refusal is not a crash.** Every refusal must reach the operator in their
   language through the existing single renderer, on the same screens that today
   return nothing at all.
9. **The rate validation is a real chokepoint, so `activate_tax` had to join it.**
   `create_tax` and `update_tax` were not the only two places that write the `rate`
   column: `activate_tax` reached `TaxRepository::update` directly with
   `current.rate`, so the ceiling had a third way around it and the "single
   production chokepoint" claim was false as written. `activate_tax` now calls
   `validate_rate` before persisting. It cannot INTRODUCE an over-ceiling rate — it
   re-persists what is already stored — but a legacy over-ceiling row (whose
   existence in the real deployment is **unverified**, because the database was
   deliberately never read) could otherwise be reactivated and re-persisted, which is
   exactly the row no other path can produce any more. Refusing is **recoverable**:
   `deactivate_tax` and `delete_tax` never look at the rate, `update_tax` accepts any
   rate at or under the ceiling, so the remedy is one form field — lower the rate,
   then activate. The refusal is an `AppError::Validation`, which
   `taxes_refusal_response` already answers 400 with the ceiling's own localized
   sentence, so no new status and no new render path was needed. Note this also means
   an over-ceiling row cannot be edited for its NAME or CODE either, because
   `update_tax` validates the effective rate — a deliberate consequence of the same
   chokepoint, and the same one-field remedy covers it.

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

### T3 — document-level accumulation (NOT STARTED, and it is the open violation)

- Make the cross-line accumulators checked, and refuse rather than panic.
- Ripple-check everything that consumes a document total.

RED first, and the RED must be the real construction, not a unit-level multiply: a
draft sale with two `unit_price = 4e28` lines, then a `GET /sales/{id}` that panics
today. A RED that only proves `a + b` overflows proves the operator's symptom is
nothing to do with this defect and will be satisfied by a fix that misses the fold.

**The defect.** Every document total is a SUM of its lines, and the sum is taken by
raw operators that no write guard covers. `SalesService::tax_split`
(`sales.rs:200-202`) and `PurchaseService::tax_split` (`purchases.rs:183-191`) fold
`net +=`, `tax +=` and `total += tax_inclusive_total(..)`, and the two
`list_document_rows` folds do the same at `sale_repo.rs:1023` and
`purchase_repo.rs:921`. Per-line carryability — everything T1 and T2 established —
says nothing about the sum. rust_decimal's raw operators panic, this crate has no
`catch_unwind`, and `Cargo.toml` has no `[profile]`, so the default unwind applies.

**Four reachable surfaces**, all HTTP, none requiring a crafted request:

1. `GET /sales/{id}`;
2. the add-line response itself, on the second line: `add_line_impl` →
   `record_context` → `get_record`;
3. `GET /purchases/{id}`;
4. the documents list page, through the two `list_document_rows` folds.

**The worst part, and the reason this cannot be deferred casually:** the second
line's INSERT COMMITS before the response renders, so the panic does not roll the
write back. The operator is left holding a document that is permanently unreadable
— every one of the four surfaces above panics on it, and nothing in the application
can open, print, confirm, cancel or take payment for it. The data is not merely
unrenderable; it is stranded.

**The honest fix shape.**

* `tax_split` (both families) and the two `list_document_rows` folds become CHECKED,
  and an overflow is mapped to a **THIRD distinct `PriceRefusal`**, not shared with
  either line-amount refusal. Decision 3 forbids sharing for the same reason it
  separated the first two: the remedy is a different operator action. A
  line-amount refusal is "lower the quantity or the unit price". A document-total
  refusal is "this document is too large to total as a whole" — the operator must
  reduce the document, split it, or have it corrected at the source. One variant
  would tell the operator to fix a number that is already fine.
* Ripple-check, because every one of these consumes a `tax_split` total and cannot
  be left raw by accident: `SalesService::totals` and
  `PurchaseService::totals`, `assemble_detail` on both families,
  `record_from_detail` on both families, and the confirm, cancel and payment paths
  (each payment ceiling, overpayment refusal, due balance and debt figure is
  measured against the tax-inclusive total). Every raw `+`/`-` on a document total
  is a candidate for the same overflow, including `total - paid`.

**Why bounding the WRITE at the document level is NOT the fix**, stated so a future
maintainer does not take the cheap-looking shortcut:

* it does not survive a direct SQL insert. A migration, a bulk import, an admin
  repair script or any future repository method that writes `sale_lines` /
  `purchase_lines` without the write bound re-creates the exact panic, and the
  "document invariant" would be a claim with nothing enforcing it;
* the T2 census cannot be extended to cover it. The census reads one row at a time,
  and the property T3 needs is a statement about a SET of rows — a per-line census
  cannot pin a document invariant, because the per-line facts it can observe are all
  individually true for the two-line counter-example;
* it is the wrong layer. The arithmetic is a READ-side sum, and rust_decimal's
  operators are total or they are panics; the only layer that can make the sum total
  is the one that performs it.

So: CHECK THE ACCUMULATION, do not bound around it.

## Acceptance criteria

- [x] **No code path reachable from an HTTP request can panic on money arithmetic.**
      — **was VIOLATED after T1, and T3 closed it.** T1 made the per-LINE contract
      total, which left the document-level folds raw: two individually carryable lines
      of `4e28` overflowed `tax_split`'s `+=`, and because the second line's `INSERT`
      commits before the response renders, the sale was left permanently unreadable.
      T3 checked the document folds, the quantity folds and the money aggregates, and
      three further verification rounds each found one more reachable panic of the same
      class — the ageing cross-bucket sum, the purchase annulment that wrote before
      refusing, the transaction update projection, and `max - stock` in the reorder
      suggestion. All are closed. `purchases.rs:1543` remains a raw multiplication and
      is recorded above as still open, so this criterion is met for every path that
      money or quantity arithmetic reaches, and not yet for that one.
- [x] `calculate_line_taxes` cannot panic, and its totality is proven by a test, not
      asserted in a comment. — **T1, `fc1b0da`.** Per line and per calculation; the sums
      a document folds are T3.
- [x] Every refusal reaches the operator as a localized message, on the sale, purchase
      and product paths. — **T1, `fc1b0da`;** extended in T3 to the finance, stock and
      list surfaces, all through the one renderer and the one mapping.
- [x] A rate above the ceiling is refused; a rate at the ceiling is accepted. —
      **T2, `b12e1b7`.** Enforced on create, on update, and on `activate_tax`
      (Decision 9).
- [x] The closed-set refusal and localization tests still pass with no duplicate and
      no missing row. — **T1, T2 and T3.**
- [x] No new money rounding rule and no second final-price formula. — **T1, verified
      again at T3.**
- [x] No test panics anywhere in the suite. — **1203 pass, zero `#[should_panic]`,**
      and no test can panic on a legitimate input. This criterion is only as good as the
      last verification, which is the reason it was verified three times.

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
  Neither this nor `purchases.rs:487` is *tax* arithmetic, so they are not the
  acceptance criterion's violation. **The criterion IS violated, by `tax_split` and
  the two `list_document_rows` folds — that IS tax arithmetic, and it is T3.** The
  statement that the criterion "holds as written" was itself part of the over-claim
  corrected during T2 verification: it is withdrawn. All four sites are the same
  family and all four are unfixed.
- **A rate ceiling changes what is storable, and T2 widened that by one path.** An
  existing deployment with a stored rate above the ceiling would still load, but could
  no longer be updated through Settings, and as of Decision 9 it can no longer be
  REACTIVATED either. Whether that is acceptable is a data question, and the data was
  not read. The mitigation is that the refusal is recoverable through one form field,
  and that `deactivate_tax` and `delete_tax` never consult the rate at all.
- **The read-path invariant is a single point of failure, and it is only per line.**
  If a future write path or a migration inserts an unrepresentable amount directly, the
  read multiplies panic, and T2's census is the guard against that. But the census is
  proven load-bearing for the LINE only: two individually carryable lines still panic
  in the document fold, and no extension of a per-line census can see that. T3 is the
  real fix, and until it lands the invariant is a statement about a row, not about a
  document.
- **Whether a panic inside the open transaction leaves the connection usable is
  unverified.** This feature removes the trigger, not the uncertainty.

## Progress

- Evidence gathered by read-only investigation; the census, the blast radius, the
  limits, and the two raw-operator sites are confirmed above.
- Independent verification of T1 fuzzed 400,000 `(net, rate-set)` pairs and 800,000
  operand pairs with zero panics and found no new warnings.
- Independent verification of T2 found the work behaviourally correct and green, but
  raised four claims and gaps. All four are corrected: the census documentation now
  states per-line carryability and names the document-level gap (which is now T3 and an
  OPEN acceptance criterion), the ceiling argument is corrected from `MAX/11 ≈ 7.2e27`
  to `MAX/1000 ≈ 7.92e25` with the reason (the contract multiplies before dividing by
  100), the same over-claim in `line_taxes.rs` is corrected to "the tighter of the two,
  and the per-multiply binds above ≈1.01%", and `activate_tax` now goes through
  `validate_rate` so the chokepoint claim is true (Decision 9).

### T1 — contract total + five sites

**LANDED, committed as `fc1b0da` — `fix(taxes): refuse instead of panicking on tax arithmetic overflow`.**

- `calculate_line_taxes` returns `Result<LineTaxCalculation, PriceRefusal>`; every
  step inside it is checked, including the final add.
- `line_net_amount` makes the raw `qty * price` argument checked, at the four
  repository sites that computed it before entering the contract.
- Five production call sites propagate the refusal instead of panicking.
- Two new variants, `LineAmountTooLarge` and `TaxArithmeticTooLarge`, with EN and ES
  catalog rows wired through `price_refusal_key`. The product drawer answers 200 with
  the refusal rendered rather than an error, so a storable product stays reachable.
- Tests: 1161 pass, no test panics. e2e 112 passed / 4 skipped.
- **What T1 did NOT establish, and this is now recorded rather than glossed:** the
  read path's safety argument is per LINE. T1's own commit message repeated the
  over-claim corrected in T2 — it says "for a non-negative rate set the pair is
  tighter", which is false (see Decision 6). The message cannot be amended without a
  history rewrite, so the correction lives here, in `line_taxes.rs` beside the
  arithmetic, and in the constant's own doc, which is where a maintainer reads it.

### T2 — rate ceiling + write-bound invariant

**LANDED, committed as `b12e1b7` — `feat(taxes): bound the storable tax rate and pin the per-line read bound`.**

- `src/services/taxes.rs` — `MAX_TAX_RATE_PERCENT` (1000%), the
  `TAX_RATE_ABOVE_CEILING` marker, the second rule in `validate_rate`, and
  `activate_tax` routed through `validate_rate` (Decision 9).
- `src/localization/mod.rs` — `ValidationTaxRateTooHigh` plus the EN and ES catalog
  rows.
- `src/routes/settings_web.rs` — the marker mapped to the message key by
  `tax_error_message`, which is what makes the refusal an operator sentence instead of
  an internal marker.
- `src/tax_tests.rs` — the bound proven on both sides, on both the create and the edit
  path, the negative rule preserved, and `activate_tax` refused on a legacy
  over-ceiling row with the recovery proven; the ceiling number pinned across the
  constant, the marker and both catalogs.
- `src/settings_tests.rs` — the refusal reaching the operator in their own language on
  the real form, both languages, plain render and HTMX, with the no-write assertion
  read AFTER both posts so the HTMX branch is genuinely covered.
- `src/tax_snapshot_tests.rs` — the write-bound census over every stored line of both
  families, plus a counter-example that smuggles a row in through SQL to prove the
  census can fail. Documentation states per-line carryability and names the
  document-level gap.

### T3 — document-level accumulation

**LANDED, committed as `ee9a5be` with a maintainer-approved `size:exception`.** 45
files, +5883/−887 — roughly 17× the 400-line review budget. The exception was granted
by the maintainer rather than the code being shrunk to fit. A hunk-level split was
examined and rejected as dishonest: the test modules of `sales.rs`, `purchases.rs` and
`inventory_web.rs` each hold both a fold and the rendering that shows it, so neither
half compiles or passes alone.

What landed, and the real construction that proved each one:

| Panic, reachable over HTTP | Fixed by |
| --- | --- |
| Two carryable lines of `4e28` overflow the document total; the second `INSERT` commits before the render, so the sale is stranded | `tax_split` and `paid_and_due` checked, `DocumentTotalTooLarge` |
| Two sales in different ageing buckets each fit, the cross-bucket sum does not | `Ageing::total()` folded checked; the old test used `draft_sale`, whose fixed due date put both in one bucket |
| `qty = 4e28` at cost `0`: the document total is legitimately `0`, so no money bound fires, and `tracked_units` overflows | `tracked_units` as a checked `SetMoney` — a money bound is not a quantity bound |
| `existing.qty + qty` in the scan-merge path | checked before the update |
| Purchase `cancel` applied the reversal, then refused | the money resolves before any write, as the sales twin already did |
| Stock level and account balance folds, found by a sweep of all of `src/` | `checked_aggregate_sum` with `AggregateTooLarge` |
| `PUT /api/transactions/{id}`: `current_balance - orig_signed` with an empty body | checked projection, refused before the write |
| `max - stock` in `suggestion_for`, which took down the whole catalogue | `checked_sub`; `None` already meant "no suggestion" |

Also in this unit: a refused aggregate renders **in place** rather than refusing the
page, on every list surface, through one renderer and one mapping. A row that renders a
zero or a partial figure is worse than one that renders nothing, because the operator
cannot tell a real zero from a refusal — so the figures travel as `String`, empty when
refused, and a template cannot print a number that is not there. `ORDER BY id` landed on
both aggregate folds, which makes the pre-check and the fold walk the same prefixes
rather than a planner's choice of date order.

Two induction comments that were load-bearing and wrong are replaced with what is
actually true: **the fold is the guarantee**, and the write pre-check buys an early
refusal with a useful message, not unreachability.

Tests: 1203 pass, no test panics, zero `#[should_panic]`. e2e 112 passed / 4 skipped.
No new warning kinds; two baseline warnings are gone.

### Still open, in this family and not fixed here

- **`purchases.rs:1543` `suggested_qty * unit_cost` is a reachable raw multiplication.**
  Both operands are bounded only by representability, so the product is not. B2
  removed the refused-level path into it; nothing else did. Fixing it needs a decision
  about whether a supplier cost or a reorder ceiling may be that large at all — a
  product decision, not a mechanical fix.
- **`max_stock` has no ceiling.** A product can be created whose reorder arithmetic is
  unrepresentable. It is now readable and suggestion-free rather than fatal; the data
  is still odd and only a product decision changes that.
- **The public accounts API changed shape for a refused balance**: `balance` becomes
  `string | {"refused": "..."}` and `cached_balance` is omitted. Nothing in this repo
  reads either field, so the repo cannot say whether an external consumer needs a
  changelog note.
- **Stored production data was never read.** Whether any real deployment holds a rate
  above the ceiling or an unrepresentable amount is unverified.
- **Whether a panic inside an open transaction leaves SQLite usable** was never
  verified against sqlx 0.9. This feature removes the triggers, not the uncertainty.
