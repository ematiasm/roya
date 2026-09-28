# Purchases: a search-only entry row

## Status

**Design closed. Four decisions taken; four tasks to build.** Cut from `main`
at `181f64e`, after `cost-with-taxes` merged as #120, #121, #122 and #123.

## Objective

The entry row carries **only the product search**. Quantity and money are edited
on the line, never before it. A bare scan or a bare accept adds the product as a
line in one step.

## What already works — this bounds the work

Three of the five things the operator asked for are not new work. Read from the
tree, not inferred:

1. **Barcodes work end to end today.** `product_barcodes` is a 0..N table, not a
   column (`migrations/20240101000005_create_product_barcodes.sql:2-8`).
   `match_catalogue` folds `normalize_search(&barcode.code).contains(&needle)`
   into the search (`src/services/inventory.rs:607-613`), and
   `resolve_product_ref` tries the **exact barcode before the SKU**
   (`:656-660`, then SKU at `:661`, then id at `:664`). The keyboard-wedge rule
   already exists by name in `templates/base.html:419-435`: a printable
   character arriving while a result button is focused clears `#product-picker`
   and refocuses it, so scan characters are not swallowed by the button.
   `e2e/tests/test_picker.py:410-457` already proves a repeat scan merges into
   one line. **No schema change, no field, no endpoint.**
2. **A new line already shows its resolved cost.** `resolve_line_cost`
   (`src/services/purchases.rs:486-507`) resolves supplier satellite → product
   column when no explicit cost is given, and that resolved `Decimal` is what
   `write_line_with_taxes` **writes** (`src/repositories/purchase_repo.rs:662-672`).
   The add response re-reads the record from storage
   (`web_add_line_impl:1539` → `record_context`), and the line's net input renders
   it (`templates/partials/purchase_detail.html:241`). Proved by
   `e2e/tests/test_purchases.py:400`. **The operator already sees the existing
   cost, already editable.**
3. **The line already edits qty, net and gross in one PUT.** `hx-put` plus one
   `hx-include` naming all four ids (`purchase_detail.html:229-268`), read by
   `UpdateLineForm` (`src/routes/purchases_web.rs:1293-1309`). Covered by
   `a_gross_typed_into_an_inline_edit_stores_the_solved_net` (`:8490`),
   `an_inline_edit_with_neither_cost_is_still_refused` (`:8517`),
   `a_refused_gross_edit_stores_nothing_and_answers_the_shared_sentence` (`:8607`),
   and the two matching e2e tests.

So the delta is: remove three inputs from one template, delete what they owned,
and fix one blocker.

## The problem

Loading a purchase asks for a quantity and a cost **before the product is even
accepted**. The cost of that ordering is not one extra click per line, it is a
different shape of work:

- The operator has to decide a quantity before they have decided they want the
  product at that quantity, so the number gets typed twice — once to add, once to
  correct.
- A repeated scan re-opens the quantity question for a product already on the
  purchase, when the only intent is "one more".
- The cost is asked for in a place where the operator cannot see the product's
  own existing cost, and `resolve_line_cost` already has that answer.

The scan workflow this fights is the one the receiving desk was built for: find,
accept, move on. Money is a second pass on a line that already exists.

## The blocker

**`value="1"` on the entry row's quantity is markup only. The server has no
default.** `AddLineForm.qty: String` is `#[serde(default)]`, so a missing field
arrives as the empty string, and `parse_required_decimal` rejects it
(`purchases_web.rs:1463`). Remove the input without adding a default and **every
bare scan becomes a 400**. This is the one change that must exist for the
operator's requested flow to work at all.

## Decisions

1. **The default lives at the web form boundary, not in the service.** `AddLineForm.qty`
   becomes `Option<String>`; an absent quantity resolves to `Decimal::ONE` in the
   route. `add_or_increment_line` keeps its `qty: Decimal` and its
   `qty > 0` invariant absolutely, and the API DTO keeps requiring an explicit
   quantity. The service's rule never loosens; only the form, which is looser
   than the service, acquires a default.
2. **A repeat scan increments by one.** With the default at 1, the existing merge
   (`add_or_increment_line`, `checked_add` on quantities) preserves the behaviour
   `test_picker.py:410` already asserts. A scan is "one more", never "a second
   line".
3. **The wire contract is retained.** `AddLineForm` and `UpdateLineForm` keep
   `unit_cost`, `unit_cost_gross` and `cost_basis`. They become **server-only**:
   accepted on the wire, absent from the markup. REST clients and any
   non-browser caller keep working, and the four form-post tests that exercise
   the refusals and the merge survive untouched.
4. **No line-total input.** The operator asked, and was shown the arithmetic:
   `121,00 ÷ 3 = 33,33`, re-priced `3 × 33,33 = 120,99`. The cent is gone and
   every later update rounds the same way. The decision is **net and gross,
   both editable; the total is derived and shown**. This is the settled decision
   of `odd/tasks/cost-with-taxes.md` and it is not reopened.

## What is removed, and why it is not a capability loss

`cost-with-taxes` T-C gave the entry row a live server-computed cost preview.
With the money fields gone it has no caller, and three pieces of state exist
**only** on the entry row:

| removed | why it goes |
|---|---|
| `GET /web/purchases/{id}/lines/cost` + its collection twin | the only caller was the entry row's `hx-get`. No caller, no endpoint. |
| the mirror JS and `data-last-edited` in `templates/purchase.html` | it guards a **loop between two mirroring inputs**. The line row has no mirror: its PUT's answer *is* the record re-rendered from storage, which is the stronger server-authoritative pattern T-C already chose for that row. A guard on a loop that cannot happen is dead weight. |
| `#line-cost-refusal` / `[data-cost-refusal]` | the refusal surface for a field that no longer exists. The line row's refusal arrives as the shared 400 sentence on its own PUT, and that path is already tested. |

This is the elimination of a capability, not a regression. What the operator
loses is **live** typing-time feedback on the entry row; what they keep is both
figures, server-computed, on the line.

## Tests that must change, and how

Read from the tree. Grouped by why they fail.

**Assert a field exists — the assertion is now wrong, not the code:**

| test | file:line |
|---|---|
| `assert_entry_row_is_empty_and_focused`, loop over `id="line-qty"` / `id="line-unit-cost"` | `src/routes/purchases_web.rs:2335-2376`, assertion `:2370-2375` |
| draft-record entry-row field test | `src/routes/purchases_web.rs:4261-4267` |
| `assert_oob_picker_is_empty_and_focused`, same loop | `src/smoke_tests.rs:4562-4856`, assertion `:4850`; callers `:4726`, `:4737` |

**Belong to the entry row's money fields and are deleted with them:**

| test | file:line |
|---|---|
| `the_entry_row_preview_answers_the_counterpart_in_both_directions` | `purchases_web.rs:7739-7790` |
| `the_preview_reports_a_staircase_gap_as_a_refusal_in_its_body` | `:7795-7817` |
| `the_preview_is_silent_while_there_is_nothing_to_solve` | `:7824-7856` |
| `the_preview_answers_the_displayed_figure_and_reads_back_through_the_write` | `:7862-…` |
| `the_cost_preview_is_gated_on_the_purchases_read_permission` | `:7932` |
| `a_gross_typed_into_the_entry_row_is_stored_as_typed` | `:7996` |
| `a_net_typed_into_the_entry_row_is_stored_as_typed` | `:8035` |
| `test_a_gross_typed_into_the_entry_row_fills_the_net_and_stores_it` | `e2e/tests/test_purchases.py:1353-1411` |
| `test_a_net_typed_into_the_entry_row_fills_the_gross` | `:1414-1433` |
| `test_a_gross_the_operators_own_figure_keeps_what_they_typed` | `:1436-1524` |

**Split, not deleted** — `test_a_gross_that_is_the_gross_of_no_net_says_so_and_stores_nothing`
(`:1527-1563`): the `[data-cost-refusal]` half dies with the entry row, and the
"the write refuses and stores nothing" half already exists on the line row as
`a_refused_gross_edit_stores_nothing_and_answers_the_shared_sentence`
(`purchases_web.rs:8607`). Verify the overlap before removing either.

**Survive unchanged**, because they post the wire contract rather than drive the
DOM: `every_cost_refusal_answers_the_shared_sentence_on_the_write` (`:8118`),
`a_cost_refusal_renders_in_the_active_locale` (`:8286`),
`a_gross_typed_on_create_reaches_the_same_cost_resolution_a_net_does` (`:8339`),
`the_same_product_added_by_gross_and_by_net_merges_into_one_line` (`:8442`),
`the_stated_basis_decides_when_both_figures_are_on_the_form` (`:8551`).

**Drives the entry row's quantity and must be rewritten to the new flow**, not
weakened: `test_the_picker_island_owns_the_purchase_search`
(`e2e/tests/test_picker.py:346`, sets `#line-qty` to 3),
`test_choosing_a_result_on_the_purchase_page_renders_and_adds` (`:390`, sets 4),
`test_a_repeat_scan_on_a_draft_purchase_merges_into_one_line` (`:431,439`, sets 1).
Each of these encodes "the operator can set a quantity at add time" — which is
exactly what this feature removes. They become "the line arrives with quantity 1
and the resolved cost", and the repeat-scan test keeps asserting the merge.

**Not affected:** everything on the sale page. `#line-qty` and
`#line-unit-price` also exist in `templates/partials/product_search_results.html:84-85`,
and `test_picker.py:69,80,102,187,197,210,251,276` and `test_search_ux.py:261,519`
drive the sale. Do not touch them; a test that says `#line-qty` may be targeting
the sale partial.

## Tasks

- [ ] **T1 — the default, and the RED that proves it is needed.** A test that
  posts the add form with **no** `qty` field at all and asserts the line is
  created with quantity 1. RED against `main` with a 400. This is the task the
  whole feature stands on; do not start the template work before it is green.
- [ ] **T2 — the entry row loses its money fields.** Remove `line-qty`,
  `line-unit-cost`, `line-unit-cost-gross`, `line-cost-basis` and
  `line-cost-refusal` from `#line-add-form`, keeping the search, the hidden
  `product_id` and the submit. Then rewrite the three tests that assert those
  fields exist, and the three e2e tests that drive them. Keep the field the
  picker owns.
- [ ] **T3 — delete what the fields owned.** The preview endpoint and its
  collection twin, the mirror JS and `data-last-edited` in `templates/purchase.html`,
  the refusal slot, and the ten tests listed above. Confirm each is unreachable
  before removing it — `grep` the route path and the `data-cost-refusal` hook.
- [ ] **T4 — prove the flow the operator asked for.** One browser test that finds
  a product, accepts it, and asserts the line arrives with the **product's
  existing cost already in the net input**; one that scans a barcode with no
  further input and asserts the same; one that edits qty and gross on that line
  and asserts the pair re-renders from storage. Each proved by a mutation.

## Constraints

- **The service's `qty > 0` invariant does not loosen.** The default is a form
  concern.
- **No new money arithmetic.** `round_to_cents` and `calculate_line_taxes` stay
  the only definitions; `solve_net_from_gross` stays the only gross→net. This
  feature moves no money.
- **`unit_cost` stays the stored truth** and the line total stays derived.
- **Do not touch the sale page** or its tests.
- **Do not touch `product_barcodes`, `match_catalogue` or `resolve_product_ref`.**
  The barcode path already works; it is read here, not changed.
- **Strict TDD, mutation-proved.** A test that survives the mutation it was
  written for is decorative.
- **Technical artifacts in English.**
- Never stage `odd/tasks/pos-counter-sales.md` or
  `odd/tasks/residual-interface-scope.md`. Never open `roya.db`; test pools are
  `sqlite::memory:`.
- **Never pin markup instead of behaviour.** The inline line edit shipped once
  with a Rust test asserting the input exists and no browser test at all, and the
  markup was right while the behaviour was broken
  (`odd/tasks/purchases-receiving-flow.md`, T1). T4 exists so that cannot recur.

## Applicable checks

`cargo test`, `bash scripts/e2e.sh`, `cargo fmt --check`, `cargo clippy
--all-targets`. The purchase e2e suites and `test_picker.py` are the ones that
observe this feature; a green Rust suite is not evidence for a DOM contract.
