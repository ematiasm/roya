# Cost with taxes — accept a tax-inclusive purchase cost

## Status

**Planning complete. BLOCKED on a merge. No implementation yet.**

## ⛔ Hard dependency: `feat/final-price-markup` must land first

This feature **cannot start on `main`**, and the reason is not organizational —
it is that the code T1 has to extract is not there.

| symbol | `main` | `feat/final-price-markup` |
| --- | --- | --- |
| `ProductPriceLadder`, `net_refusal` | present | present |
| `solve_final_price`, `max_solvable_final_price` | **absent** | present |
| `final_price.rs` (`gross_divisor`, `solve_net`) | **absent** | present |
| `line_net_amount` (the overflow guard) | **absent** | present |

`src/repositories/purchase_repo.rs:313` on `main` reads:

```rust
let calc = calculate_line_taxes(qty * unit_cost, &taxes);
```

— a raw multiply. On `feat/final-price-markup` the same line is
`line_net_amount(qty, unit_cost).and_then(|net| calculate_line_taxes(net, &taxes))`,
which returns a `SolveResult` and **refuses instead of panicking**. So `main`
still carries a reachable money-overflow defect that the unmerged branch already
fixes, and **T4 edits exactly that line**. Writing T4 on `main` would mean
editing an expression that is itself the bug.

**Task-level consequence:**

| task | blocked? | why |
| --- | --- | --- |
| T2, T5, T6, T7 — the **figure** | **no** | `calculate_line_taxes` is public at `line_taxes.rs:139` and the ladder already resolves the tax set at `taxes.rs:317`. The whole visible payoff is executable on `main` today. |
| T1 — extract the inverse | **yes** | the code does not exist on `main` |
| T3 — the basis column | **yes** | executable, but it has no consumer until T4 |
| T4 — convert at the boundary | **yes** | needs the inverse, and touches a line `main` computes unsafely |

**The decision taken: wait for the merge.** The sequence stays linear and nothing
overlaps. The visible half was available early; that is the price of a clean
sequence, and it is the right trade for a data-model change that must be
reviewable on its own.

### The merge order is forced by the topology

```
main                              23f548d
└─ feat/final-price-markup        696f66c   7 commits    65 files, +13323 −1007
   └─ feat/drawer-split-component f8b1722  11 commits    23 files,  +1937  −264
      └─ feat/cost-with-taxes     (rebase onto main after the two above)
```

`feat/drawer-split-component` **descends from** `feat/final-price-markup`, so the
second merge is 11 commits and does not re-carry the first seven.
`feat/cost-with-taxes` was cut from `main` and shares no work with either, so it
rebases cleanly once both are in.

The two feature branches are unpushed; push, PR and merge are the maintainer's
call. Nothing here was pushed, and the two `odd/tasks/` documents that must never
be staged were never staged.


## Objective

The four figures an operator needs to read together, and — the part that makes
this a data-model change rather than a display one — the ability to **load a
purchase whose costs already include tax**.

1. Cost price
2. **Cost price + taxes** ← does not exist today, in any form
3. Sale price without taxes
4. Sale price with taxes

Where sale = cost + markup.

## Why this is not the drawer consolidation

The drawer work (`odd/tasks/drawer-split-component.md`) consolidated the price
figures in the product drawer. It was scoped as a display change and it stayed
one. This feature is a different animal, and conflating the two is the mistake to
avoid.

**Today a tax-inclusive cost cannot be represented anywhere in the codebase.**
This is measured, not assumed:

| Check | Result |
| --- | --- |
| Every production `calculate_line_taxes` call against a product's tax set | `services/taxes.rs:504` (the ladder's net), `services/final_price.rs:377` and `:489` (the solve), `sale_repo.rs:344,401` and `purchase_repo.rs:315,370` (document lines). **Nothing passes a `cost_price`.** |
| Model fields | `ProductPriceLadder` (`src/models.rs:510-563`) carries `cost_price`, `markup_pct`, `net_price`, `net_is_derived`, `net_refusal`, `inputs_unreadable`, `from_form`, `breakdown`, `tax_total`, `total`. **No grossed cost.** |
| Columns | `products` carries `sale_price`, `cost_price`, `markup_pct` only. `product_supplier_costs` carries `current_cost`, `previous_cost`, `is_preferred`, `supplier_sku` — **two price columns and zero tax columns.** |
| Naming | No `gross` / `cost_with_tax` / `landed_cost` / `cost_inclusive` identifier in `src/`, `templates/`, `migrations/`. |
| The contract itself | `services/line_taxes.rs:156-160` states the decision verbatim: *"the user decision is that both are net"*. The entered `unit_cost` is fed straight into `calculate_line_taxes` as `net` — no division, no inversion, no `divisor` anywhere on that path. |

So a supplier who invoices tax-inclusive is currently **inexpressible**. That is
the gap, and it is a business fact, not a missing feature.

## The business fact, verbatim

> "al cargar una compra hay proveedores que discriminan impuestos y otros no,
> entonces poder cargar precio costo con o sin impuesto facilita mucho la carga de
> compras"

That sentence decides the design. The pain is in **entry**, not in display — a
derived `cost + taxes` figure alone would not address a word of it.

## Decisions, settled

1. **The basis is declared PER PURCHASE.** `purchases.prices_include_taxes`,
   `BOOLEAN NOT NULL DEFAULT 0`. `suppliers.prices_include_taxes` exists only to
   **pre-fill** the control; the calculation reads **only** the purchase's own
   flag. A supplier-level *declaration* was considered and rejected: if a
   supplier's practice changes, every subsequent cost is silently netted wrong,
   with no error and no notice. A pre-fill that the operator confirms costs one
   click and cannot rot.
2. **`cost_price` stays NET.** The stored truth does not change. The entered
   gross is converted **once**, at the purchase boundary, and everything
   downstream — markup derivation, document snapshots, the ladder — keeps reading
   a net it already understands. This is what `final_price.rs:73-77` means by
   "the net price is the stored truth".
3. **The four figures are the drawer's business, not this feature's.** The ladder
   rows and the consolidated view belong to the drawer work; this feature supplies
   `cost_total` and its refusal, and the drawer renders them.

## The conversion, and why it is a cent search

`final(net) = round2(net + Σ round2(net · rate_i / 100))` is a **staircase**, not
a function with a closed-form inverse — `final_price.rs:50-69` is emphatic about
this, and states that a plain division is wrong about the exact cent "often
enough to be useless". So a gross does not divide into a net; it must be
*searched*, with every candidate verified through `calculate_line_taxes` before
it can win. `gross_divisor` places the window and `search_radius_cents` proves
the window is wide enough.

**Fidelity cost, named:** because the conversion is a search, the gross the
operator typed is recovered as `net + tax_total` and can be **one cent** away
from what they typed. Storing net is the right trade (it keeps one truth
everywhere), but this is a real loss and it should be a decision, not a surprise.
Reprinting a purchase shows the net the taxes were computed from, not the digits
the supplier wrote.

## The reusable core, and the coupling that must be cut

`final_price.rs::solve_final_price` is six rules. Two of them are exactly the
conversion this feature needs, and four are sale-specific:

| rule | line | reusable for a cost? |
| --- | --- | --- |
| 0 — ceiling `max_solvable_final_price()` | `:338` | **yes**, the arithmetic is the same |
| 1 — `stored_markup_pct.is_some() && cost_price <= 0` | `:343` | **no** — pure markup state, and it would fire on a cost-only conversion |
| 2 — `gross_divisor(taxes)` | `:351` | **yes** |
| 3 — `validate_effective_prices(kind, final_price, cost_price)` | `:354` | **no** — applies "a Product must sell for something" to a grossed **cost**; a product with a valid net sale price and a `0.00` cost would be refused |
| 4 — `solve_net(...)` | `:357` | **yes** |
| 5 — the markup | `:361` | **no** |

So: extract rules 0 + 2 + 4 into a public
`solve_net_from_gross(gross: Decimal, taxes: &[Tax]) -> SolveResult<Decimal>`,
and have `solve_final_price` **call** it. That is the point — one definition of
"which net grosses to this figure", not a second copy of a staircase search. The
private helpers `solve_net` (`:439`), `tax_arithmetic_fits` (`:550`),
`gross_divisor` (`:661`) and `search_radius_cents` (`:694`) move with it.

`gross_divisor` and `solve_net` are currently **private**, so this is a real
refactor of a module whose entire purpose the doc comment calls "not a pricing
mode". A cost gross *is* a persistent pricing mode, so the extraction must not
quietly turn that module into one: the shared half gets its own name and its own
doc comment, and `final_price.rs` becomes a caller of it.

## New refusals are required — the existing ones would lie

The inverse can return `FinalPriceNotInvertible`, `FinalPriceUnreachable`,
`NetPriceTooLarge` and `TaxRateTooLargeToPrice`. Every one of them is
**sale-worded**, and one leaks raw identifiers into user-facing copy:

- `PriceRefusalFinalPriceUnreachable` — *"no net price produces this **final
  price** with the linked taxes"*
- `PriceRefusalFinalPriceNotInvertible` — *"linked tax rates must add up to more
  than -100 **to solve a final price**"*
- `PriceRefusalNetPriceTooLarge` — *"the linked tax rates gross this
  **final_price** down to a **net_price** that is too large to store"*

Reusing them would tell an operator who typed a **cost** that their *final price*
is wrong. The house rule is that a refusal names the thing that is actually
wrong, so new variants are needed with sentences that name a cost. Each one adds
an EN and an ES catalog row, and `PriceRefusal::ALL` is walked by the
catalog-translation test, so an untranslated variant fails the build.

## The ladder's second refusal slot

`ProductPriceLadder.net_refusal` is **one slot by design** — `models.rs:541-544`
says a second field "would need a second message key and a second branch, and a
reader would still be asking the same question of both". That reasoning holds
for the net's two halves, which mean the same thing to a reader.

It does **not** hold here. `validate_effective_prices` (`services/inventory.rs:
100-121`) enforces exactly three things — `sale_price > 0` for a Product,
`sale_price >= 0` for a Service, `cost_price >= 0` — and **never** compares
`cost_price` to `sale_price`. So a manual-price product can legitimately carry
`cost > net`, and on such a product the cost gross can overflow where the net
gross does not. "There is no net to show" and "there is no cost-with-taxes to
show" are **different facts about different numbers**, and collapsing them would
hide a reachable failure. Two slots, two sentences, and a comment saying why this
case is not the one `net_refusal` was built for.

## Tasks

- [ ] **T1 — extract the shared inverse.** `gross_divisor`, `solve_net`,
  `tax_arithmetic_fits` and `search_radius_cents` move into a public
  `solve_net_from_gross`; `solve_final_price` calls it and keeps its own five
  sale rules. **Behaviour-preserving for the existing solve** — the mutation
  test is that every current `final_price` test still passes unchanged.
- [ ] **T2 — the cost-side refusals.** New `PriceRefusal` variants with EN + ES
  sentences that name a cost, added to `PriceRefusal::ALL`, proven by the
  catalog-translation test.
- [ ] **T3 — the purchase basis column + control.** `purchases.prices_include_taxes`
  in a migration, the `suppliers` pre-fill column, the control on the purchase
  form, and the plumbing from the handler. Nothing computes anything yet.
- [ ] **T4 — convert at the boundary.** `purchase_repo.rs:313` and `:368`: when
  the purchase's flag is on, net the entered `unit_cost` before
  `line_net_amount`, and store the net. Both the create and draft-edit paths, or
  the two will disagree.
- [ ] **T5 — the ladder figure.** `ProductPriceLadder.cost_total` +
  `cost_refusal`, computed by `calculate_line_taxes(cost_price, &taxes).total`
  — **zero new arithmetic**, the contract already exists. A refused cost
  publishes no amount, as an empty `String`, so a template cannot print a number
  that is not there.
- [ ] **T6 — the ladder row.** In the drawer work's `product_price_ladder.html`,
  as a **4-`<td>` `<tr>`**: `e2e/tests/test_products.py::_ladder_amounts` keys
  every figure by `td[0]` and reads `td[3]`, and **silently skips rows with
  fewer cells**. A row that is not a 4-cell `<tr>` is invisible to all six
  browser tests.
- [ ] **T7 — tests.** The purchase path in both directions, the staircase
  (a gross that is the gross of no net must refuse, not round), the rejection
  case where the rate set is not invertible, and a mutation proving each.

## Constraints

- **No new money rounding rule.** `round_to_cents` stays the only rounding;
  `calculate_line_taxes` stays the only definition of a gross.
- **One definition of the inverse.** `solve_net_from_gross` is the only place a
  gross becomes a net. If the purchase path grows its own division, the
  staircase is bypassed and the cent is wrong.
- **`round_to_cents` and `calculate_line_taxes` are untouched by T6.** This
  feature moves no money and computes no price; it makes an existing computation
  reachable from a new input.
- **A refused figure publishes no amount** — not a zero, not a partial, not a
  stale one.
- **Strict TDD.** `cargo test` plus `bash scripts/e2e.sh`; the purchase entry path
  is user-visible, so e2e is not optional. Prove each test by mutation, not by a
  green suite — a test that survives the mutation it was written for is
  decorative.
- **Technical artifacts in English.**
- Never stage `odd/tasks/pos-counter-sales.md` or
  `odd/tasks/residual-interface-scope.md`. Never open `roya.db`; test pools are
  `sqlite::memory:`.

## Two facts that will mislead a reader

1. **`taxes` is EMPTY in production.** Migrations seed nothing and
   `src/t1_schema_tests.rs:164-175` asserts it. Every `IVA 21` / `IVA 10,5` in the
   tree is a **test fixture**. Do not plan around a default tax catalogue; there
   is not one.
2. **The tax model has no heterogeneity to complicate the inverse.** One `rate`
   column, percentage of net, `0…1000`, no `basis` column and no fixed-amount
   tax. "EXENTO" is just a row with `rate = '0'`, and it is a real linked tax —
   the arithmetic still runs. So every rate set here is invertible or not as a
   whole, and the only failure modes are the staircase gaps and the ceiling.

## Branch and delivery

New branch from `main`, **not** from `feat/final-price-markup` or
`feat/drawer-split-component`. This feature is a data-model change and must be
reviewable on its own; the other two branches are unpushed and each already
carries a second concern. Cutting from `main` avoids inheriting either.

Size is a real risk here: the cost-feature history in this repo is
`odd/tasks/cost-price-freshness.md` at 40K, so expect this to exceed the ~400
line heuristic and plan the PR structure deliberately rather than discovering it
at the end.
