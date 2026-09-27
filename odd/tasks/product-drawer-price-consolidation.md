# Product drawer — consolidate the price figures

## Status

**Investigation complete. No implementation yet.** This document is the handover from the
session that mapped it; the next session starts here, not from zero.

## Objective

Show all four price figures together in the product drawer, so the operator can read the
whole price picture in one place instead of assembling it across three regions.

1. Net cost
2. Cost plus taxes
3. Sale price without taxes
4. Final price the customer pays

## The finding that changes the scope

**Figure 2 does not exist anywhere in this codebase.** Verified four independent ways:

| Check | Result |
| --- | --- |
| Every production `calculate_line_taxes` call against a product's tax set | `services/taxes.rs:504` (the ladder's net), `services/final_price.rs:377` and `:489` (the solve), `sale_repo.rs:344,401` and `purchase_repo.rs:315,370` (document lines). **Nothing passes a `cost_price`.** |
| Model fields | `ProductPriceLadder` (`src/models.rs:510-563`) carries `cost_price`, `markup_pct`, `net_price`, `net_is_derived`, `net_refusal`, `inputs_unreadable`, `from_form`, `breakdown`, `tax_total`, `total`. No grossed cost. |
| Columns | `products` carries `sale_price`, `cost_price`, `markup_pct` only — `migrations/20240101000004_create_products.sql:3-20`, `20240101000031_add_audit_inventory.sql:99-119`, `20240101000035_add_product_markup.sql:12`. |
| Naming | No `gross`/`cost_with_tax`/`landed_cost`/`cost_inclusive` identifier anywhere in `src/`, `templates/`, `migrations/`. |

**So this is not a reorganisation. Three of the four figures exist and move; the fourth
would be a new figure, with a new computation, a new field, and — critically — a second
refusal slot.** That is a product decision, not a layout change, and it is the first
thing to settle before touching a template.

The reusable pieces already exist and need no new query: the `Vec<Tax>` is already
resolved at `services/taxes.rs:412` via `list_active_for_product` (`:334`), and
`calculate_line_taxes` already returns `total = round_to_cents(net + tax_total)`.

## Today: where each figure lives

Three regions, and two of the figures appear in **two** places each.

| Region | `product_detail.html` | Figures |
| --- | --- | --- |
| **Card A** — header + edit form | price row at `:105-118` (sale price, cost price), markup at `:119-121` | **editable inputs** for figures 1 and 3 |
| **Card B** — price ladder | ladder island at `:180`, final-price control at `:212-235` | figure 1 (row 1), markup (row 2), figure 3 (row 3), tax rows, tax total, **figure 4** (tfoot) |
| **Card C/D/E** | taxes, supplier costs, stock movement | no price figures |

The editable inputs sit roughly **100 lines above** the ladder that displays the same
numbers. The comment at `product_detail.html:31-33` states deliberately that every money
figure belongs to the ladder — so a consolidation has to reckon with a rule that was
written on purpose.

The ladder publishes (`templates/partials/product_price_ladder.html`): net cost `:40-45`,
markup `:46-51`, net sale price `:52-65`, a refusal row `:66-74` when `net_refusal` is
set, one row per linked tax `:76-83`, tax total `:88-93`, price with tax `:94-99`, plus
three conditional notes — no-taxes-linked `:104-108`, unreadable `:109-111`, and
no-cost-markup `:112-130`, which is deliberately **outside** the refusal guard.

## The fragility that will bite first

`e2e/tests/test_products.py:1188-1202` — `_ladder_amounts()` keys every figure by
`cells.nth(0).inner_text().strip()` and reads the amount from **`cells.nth(3)`**. It
assumes a `<tr>` with 4+ `<td>` and silently **skips** rows with fewer cells (`:1199-1200`).

**Moving a figure out of the 4-column table, out of `td[0]`/`td[3]`, or out of a `<tr>`
breaks six browser tests by name while no behaviour changed.** Six Rust route tests
assert the same figures by label and would survive a move; the Playwright ones would not.

## The visual baseline

`e2e/visual-baseline.json` is **not screenshots** — it fingerprints 19 computed styles per
element keyed by **DOM path** (`body/div[0]/…/tag[n]`), and deliberately excludes width,
height and vertical margins (`test_visual_baseline.py:120-143`).

Exactly **one** capture includes the drawer: `products-drawer` (`:411`), 399 elements.
`products` and `products-create-under-filter` are unaffected — the drawer is fetched on
click.

Consequence: the capture **count** never changes (86 keys), but moving any card reshuffles
every index downstream of it, producing up to ~399 "gone from X / appeared at Y" pairs
against a 40-problem reporting cap. Two captures, `products-drawer:hover` and
`products-drawer:hover-skipped`, compare `{} == {}` and pass **vacuously** — the drawer loop
only calls `_fingerprint`, never `_hover_fingerprint`.

## `data-action` pinning

Every state-changing form carries `data-action` so `base.html:110-136` can label the
success/error notice. The drawer's five: `ProductSave` (`:73`), `ProductPreviewFinalPrice`
(`:215`), `ProductLinkTax` (`:269`), `ProductRecordCost` (`:330`), `ProductRecordMovement`
(`:367`).

The pin is on the **form element**, so moving a form intact is a no-op. Two real risks:
the ladder island is `hx-swap="outerHTML"`, so the final-price control at `:215` keeps its
label **only** because it is a sibling of `#product-price-ladder` and not a child — moving
it inside the island would destroy its label on every refresh; and nothing in the Rust
suite pins any drawer `data-action` (`smoke_tests.rs:4508` asserts only
`["Create category", "Create product"]`).

## Localisation

All four labels exist **except figure 2**. One flat table, EN at `localization/mod.rs:1202-1296`
and ES at `:2171-2269`.

| Figure | Key(s) | EN |
| --- | --- | --- |
| net cost | `ProductCostPrice` (ladder `:41` and the field partial `:14`) | "Cost price" |
| **cost + taxes** | **none exists** | — |
| sale price, no taxes | `TaxNetPrice` (ladder `:58`), `ProductSalePrice` (field `:20`) | "Net price" / "Sale price" |
| final price | `TaxInclusivePrice` (ladder `:95`), `ProductFinalPriceLabel` (`product_detail.html:226`) | "Price with tax" / "Price the customer pays" |

Two keys label figure 4 on purpose: `product_detail.html:220-225` explains that reusing
`TaxInclusivePrice` on the input would put a "price with tax" label on a page that may be
refusing to publish any tax money.

**Two strings are coupled to the current vertical order** and will read wrong after a move:
`ProductLadderLegend` (`:1232`) and `ProductFinalPriceHelp` (`:1243`) both say *"the form
above"* and *"the ladder above"*.

## Open decisions, in the order they must be settled

1. **Is "cost + taxes" a real business figure here, and if so what is it?** It is
   arithmetically computable, but the tax set exists to gross a **sale**. Adding it means
   a new computation, a new field, and a second refusal slot — and
   `validate_effective_prices` (`services/inventory.rs:100-119`) enforces `cost >= 0` and
   `sale_price > 0` but **never** `cost <= sale_price`, so a manual-price product can have
   `cost > net`, and on such a product the cost gross could overflow where the net gross
   does not. That refusal is genuinely reachable.
2. **Consolidation means moving the editable inputs, or adding a summary block?** The inputs
   *are* how the operator edits, and the ladder is `hx-swap="outerHTML"`, so the two cannot
   simply be merged without breaking the form ownership that
   `the_final_price_control_is_separate_from_the_save_form` (`inventory_web.rs:8264`)
   pins positionally.
3. **Does the ladder's "a refusal publishes no money" guarantee extend to a cost figure?**
   It is stated only in template prose and pinned by `assert_publishes_no_tax_money`; it has
   never covered anything but the net-side triple.
4. **Should the ladder table's 4-column shape survive?** Every browser figure assertion
   depends on `td[0]` and `td[3]`.

## Constraints

- **No new money rounding rule and no second final-price formula.** `round_to_cents` stays
  the only money rounding; `calculate_line_taxes` stays the only final-price definition.
- **One renderer.** Every refusal goes through `price_refusal_key` → `price_refusal_message`.
  `only_one_place_in_the_tree_maps_a_price_refusal_to_a_sentence`
  (`inventory_web.rs:6979`) walks all `.rs` files and pins all four needles to
  `src/routes/mod.rs`.
- **A refused figure publishes no amount** — not a zero, not a partial, not a stale one. The
  house pattern is that figures travel as a `String` empty when refused, so a template cannot
  print a number that is not there.
- **`net_price` stays the stored truth.** No migration, no change to how a document snapshots
  a net.
- Strict TDD. `cargo test` plus `bash scripts/e2e.sh` — the drawer is user-visible, so e2e is
  not optional. Technical artifacts in English.
- Never stage `odd/tasks/pos-counter-sales.md` or `odd/tasks/residual-interface-scope.md`.
  Never open `/home/mamull/roya/roya.db`; test pools are `sqlite::memory:`.
- `visual-baseline.json` is regenerated deliberately and each diff audited, capture by
  capture. No pre-existing element may be silently restyled.

## State of the branch this lands on

`feat/final-price-markup`, 6 commits, **nothing pushed**. The push and the PR structure are
the user's call — the branch currently carries two separate concerns: the final-price
feature (`2c5bf20`, `c7bdb1c`) and the tax-contract overflow work (`2b3f08f`, `fc1b0da`,
`b12e1b7`, `5a8ea8c`).
