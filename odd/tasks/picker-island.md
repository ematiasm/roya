# Feature: Picker island — one owner for the product picker's state

## Objective

Give the product line picker a single owner for its client state. Today the
widget's state is spread across four owners (the DOM, `base.html`, the Askama
fragment, and smuggled `hx-vals`), which is the direct cause of the documented
hidden-id defect and of the hand-written focus and in-flight machinery in
`base.html`. After this feature the picker is one island: a state object and a
render function, with no framework and no build step.

## Problem

Read from the tree on 2026-09-24.

The picker (`partials/product_search_results.html` plus its mirror in
`partials/purchase_detail.html`) holds seven pieces of state with four owners and
no single source of truth:

| State | Owner today |
|---|---|
| `query` | the `<input>`'s `value` |
| `matches` | the server, re-rendered as HTML |
| `status` ("Searching…" / "N matches") | `base.html:285-305` mutating `textContent` |
| focused product id | `pendingResultFocus`, `base.html:258-280` |
| `line_action` / `line_target` / `price` | `hx-vals` on the input, `product_search_results.html:63` |
| `product_id` of the chosen row | `hx-vals` on **each row's own form**, `product_search_results.html:113` |
| `qty` / `price` typed before submit | the inputs, scraped back via `hx-include="#line-picker"` |

Two consequences, both already observed:

1. **The hidden-id defect** (`odd/tasks/purchases-create-and-header.md`, T3). The
   route prefers a submitted `product_id` over the typed name, and `hx-vals` only
   fills *missing* keys — so a pre-filled id silently beat a freshly typed
   supplier/product name and the line was created against the wrong record. The
   bug is structural: `product_id` is markup-owned state with fallback
   precedence, not an argument to an action.
2. **Hand-written focus restoration.** `base.html:252-280` exists only because
   htmx replaces the focused result button and its own restore only knows
   elements with an id. The comment in the tree says it outright: *"focus fell to
   `document.body` and the next key went nowhere."*

Both are the same root cause: client state that no one owns.

## Why

Not because HTMX is wrong. The measured inventory says HTMX is free on the ~10
screens that are tables and forms, and expensive exactly where overlays and
stateful widgets live. The picker is the widget with the widest blast radius
(two screens: `sale_detail.html:121,252` imports the macro, `purchase_detail.html`
mirrors it inline) and the one with an already-centralised state — its glue is
already in `base.html`, so this is the cheapest place to start and the one that
retires a documented defect class rather than one bug.

## Scope

- New: `static/picker.js` (the island, committed like `static/htmx.min.js`)
- `assets/tailwind.css` (add the island to the scan path) + rebuilt
  `static/tailwind.css`
- `templates/partials/product_search_results.html` (retire the results body and
  the per-row forms; keep `line_picker` as the widget shell)
- `templates/partials/sale_detail.html` (call sites 121, 252)
- `templates/partials/purchase_detail.html` (the inline mirror, ~44-90)
- `templates/base.html` (load the island; delete the picker focus and in-flight
  handlers)
- `templates/purchase.html:97` (the Escape stand-down for `#product-picker`)
- `src/routes/inventory_web.rs` (the JSON search route)
- Rust tests that assert picker structure: `inventory_web.rs`,
  `purchases_web.rs` (9 sites), `sales_web.rs` (2 sites)
- `e2e/tests/test_picker.py`, `e2e/tests/test_search_ux.py`
- `README.md`

Out of scope: the purchase entry row's debounced autosave and optimistic
rollback, the `products.html` markup→price derived state, the drawer state
machine duplicated across five templates, the 22-event custom bus, and the CSS
token cleanup. Those are separate features; this one changes the picker only.

## Constraints

- **No framework, no build step.** Flavor A, chosen by the user. The repo's
  `.gitignore` states the posture: *"Node artifacts must never enter this repo:
  there is no Node toolchain."* The island is plain JS in a committed static
  asset.
- **The server's add-line contract does not change.** The island owns state; htmx
  keeps doing the swap, the `hx-swap-oob` fragments and the `HX-Trigger` events
  that refresh the lists. Reimplementing OOB dispatch by hand would be a worse
  bug surface than the one being removed.
- **Tailwind v4 `source(none)`.** `assets/tailwind.css` disables auto-detection
  and scans only `../templates`. Any class that exists only in `static/picker.js`
  is purged unless the island is added to the scan path. This is a known,
  documented trap in this repo (`base.html` carries the same warning for the
  notice builder).
- **Accessibility is preserved, not re-derived.** The one polite live region
  (`#product-search-results`, `aria-live="polite"`), the `aria-live="off"` visual
  list, the `sr-only` `#product-search-status`, and the `aria-hidden` busy cue are
  an existing, deliberate design. The island must reproduce it exactly; it
  becomes easier because status is now derived from state.

## Authorized scope

The picker widget only: its state, its transport for search, its markup shell on
the sale and purchase record pages, and the tests that pin all of it. No business
rule changes, no route behaviour changes other than the additive JSON search, no
changes to the other screens.

## Locked design decisions

1. **Boundary — the whole widget.** The island owns input + qty + price + results
   + busy + status, in one `[data-picker]` container. Chosen by the user so that
   the add-line submit is island-triggered with real arguments; a results-only
   island would leave `product_id` smuggled from the input and the hidden-id bug
   alive.
2. **One state object, one render function.** `state = { query, matches, status,
   focusedProductId, selectedProductId }`. Everything visible is derived in
   `render()`.
3. **Search transport is JSON.** New `GET /web/product-search.json?q=`. The
   handler is small because `inventory_service.search_products` already returns
   `Vec<ProductStock>`, which already derives `Serialize`.
4. **The JSON carries both prices, already in display form.** `sale_price` and
   `cost_price` both travel; the island picks by `data-price-kind`. This removes
   the `price` query parameter from the wire — one fewer smuggled value. The
   strings are produced by the same `sale_price_display()` /
   `cost_price_display()` helpers the fragment calls, so the island renders them
   verbatim and the server keeps the single formatting rule. `money_display`
   normalises a stored value up to exactly two decimals, so a raw `"25"` would
   have rendered `$25` where the fragment shows `$25.00`. `stock` is deliberately
   NOT normalised: the fragment renders the raw decimal, so the wire carries the
   same raw form.
5. **One add-line form, not one per row.** The shell renders a single
   `hx-post` form containing the picker input, qty, price and a hidden
   `product_id`. The island sets that hidden input from `state.selectedProductId`
   and calls `form.requestSubmit()`. Per-row forms, the per-row
   `hx-vals='{"product_id": N}'`, and `hx-include="#line-picker"` all go away.
6. **The results container stays a sibling of the form.** Form-inside-form is
   invalid HTML; the existing comment in the macro says so. Result buttons are
   `type="button"` and the island triggers the form, so there is no ordering race
   against htmx's own listeners.
7. **Focus is derived, not restored.** `render()` puts focus on the button whose
   `data-product-id === state.focusedProductId`, falling back to the input.
   `pendingResultFocus` and the `beforeSwap`/`afterSwap` pair are deleted. There
   is no race because the island renders synchronously from state and htmx never
   swaps the island's subtree.
8. **Status is derived.** `state.status` drives the live region text; the
   `htmx:beforeRequest` handler and `searchFailed` are deleted.
9. **Tailwind scan path.** Add `@source "../static/picker.js";` to
   `assets/tailwind.css` and rebuild `static/tailwind.css`.

## Enter / barcode-scanner semantics (decided)

**The server keeps resolving the typed name.** Enter in the picker input submits
the form with the typed `product` text and the server resolves it by name, SKU or
barcode, exactly as today. Chosen by the user for the smallest behavioural
change: nothing an operator already knows how to do breaks.

This is safe now, and it was not before. The hidden-id defect was reachable
because `product_id` was **markup-owned state with fallback precedence** (`hx-vals`
only fills missing keys), so a pre-filled id silently competed with a freshly
typed name. After this feature the island sets `product_id` from
`state.selectedProductId`, so the two paths no longer compete: the typed-name
resolution runs only when the island has not selected a product. The defect dies
because the ownership changed, not because the path was removed.

Accepted cost: two resolution paths coexist (the island by click, the server by
Enter). T2 must therefore keep the server's name/SKU/barcode resolution covered
by its own test, so the surviving path is pinned rather than assumed.

## Acceptance criteria

- [ ] The picker has one state object; no picker state lives in `base.html`.
- [ ] `base.html` no longer contains `pendingResultFocus`, the picker
      `beforeSwap`/`afterSwap` handlers, or the picker `htmx:beforeRequest`
      handler.
- [ ] No `hx-vals` carries `product_id`, `line_action`, `line_target` or `price`
      for the picker; no per-row forms exist.
- [ ] Typing renders results from JSON, with the same name / SKU / price / stock
      content as today.
- [ ] Focus stays on the same product across a re-search; when it disappears,
      focus returns to the input.
- [ ] The live region announces "Searching…" while in flight and the match count
      when it lands, from state, with no extra announcements.
- [ ] Clicking a match adds the line with `product_id` taken from island state;
      the server response contract is unchanged (money region + OOB + triggers).
- [ ] A class that appears only in `static/picker.js` survives
      `scripts/build-css.sh` (proves the Tailwind scan path).
- [ ] `cargo test` green; `scripts/e2e.sh` green.
- [ ] README documents the island, its state and its boundary.

## Applicable checks

- `cargo test` (primary runner; `openspec/config.yaml` sets `strict_tdd: true`)
- `cargo test --test`-scoped filters for the touched modules while iterating:
  `cargo test inventory_web`, `cargo test purchases_web`, `cargo test sales_web`
- `cargo check --all-targets` after route changes
- `scripts/e2e.sh` and `scripts/e2e.sh -k picker` while iterating
- `scripts/build-css.sh` plus the purge assertion for the scan path

## TDD

`strict_tdd: true`. Every task writes its failing check first:

- Rust tasks: the route test asserts the JSON shape and fails on a 404 before the
  handler exists.
- Client tasks: the e2e test drives the observable island behaviour (results
  render, focus survives, the added line carries the island's product id) and
  fails before the island is mounted.
- The Tailwind scan path is pinned by a class used only in the island, asserted
  present in the rebuilt `static/tailwind.css` — red before `@source` is added.

## Tasks

- [x] T1 — Lock the wire contract and the island boundary (this document's
      "Locked design decisions" and the Enter decision resolved).
- [x] T2 — `GET /web/product-search.json?q=`: the JSON search route, envelope
      `{"query", "products"}` matching the existing API convention, plus Rust
      tests for name/SKU/barcode matching and the empty-query negative. Red first.
      Closed in `9a53c5b`.
- [ ] T3 — The island module and the **sale** page cutover: `static/picker.js`
      (state, debounced fetch, render, derived focus and status, submit via
      `requestSubmit`), the Tailwind `@source` line and rebuilt stylesheet, the
      `line_picker` shell rewrite, the `sale_detail.html` call sites, and the e2e
      test. Vertical slice, red first.
- [ ] T4 — The **purchase** page cutover: retire the inline mirror in
      `purchase_detail.html`, delete the `base.html` focus and in-flight
      handlers, drop the `purchase.html:97` Escape stand-down, and rewrite the
      structural Rust tests in `inventory_web.rs`, `purchases_web.rs` and
      `sales_web.rs` that currently assert the smuggled `hx-vals`.
- [ ] T5 — e2e sweep (`test_picker.py`, `test_search_ux.py`) and README.

## Delivery strategy

Three chained slices, because the full change touches ~10 files including two
large ones and the review workload guard applies:

- **Slice 1 — T2.** Pure addition: a new JSON route and its tests. Nothing is
  removed, nothing on screen changes, independently mergeable and revertible.
- **Slice 2 — T3.** The sale page cut over. The picker still exists in its old
  form for purchases, so the two consumers are independent.
- **Slice 3 — T4 + T5.** The purchase cutover and the cleanup: the mirror, the
  `base.html` machinery, the structural tests, e2e and docs.

## Progress

- 2026-09-24: document created. Design locked with the user: flavor A (no
  dependency, no build step) and the whole-widget boundary.
- 2026-09-24: T1 closed. Enter semantics decided: the server keeps resolving the
  typed name, because the island now owns `product_id` and the two paths no
  longer compete. Delivery agreed as three chained slices with one commit per
  task on `feat/picker-island`.
- 2026-09-24: T2 closed in `9a53c5b`. Slice 1 complete: the JSON route is a pure
  addition, so nothing on screen changed and it is independently revertible.
  The `q`/`product` alias resolution was extracted into `resolve_search_query`
  and is now shared by both routes.
- 2026-09-24: the wire format refined in `2e5051c`. Money now travels in display
  form (decision 4). The refinement was caught by asking what the island would
  actually render: a raw decimal string would have silently changed `$25.00`
  into `$25` on cutover.

## Verification evidence

- **T2, red first**: `cargo test n5_product_search_json` → 3 failed / 0 passed,
  every failure `404 route not found` (the route did not exist). Not a
  compile error and not a wrong assertion — the right reason.
- **T2, green**: `cargo test n5_product_search_json` → 3 passed.
- **T2, full suite**: `cargo test` → **889 passed** (886 before, +3 new), no
  failures. The refactor of the shared query resolution touched the existing
  HTML route, so the full suite is the check, not a filtered one.
- **Wire format, red first**: the new
  `n5_product_search_json_money_is_the_fragments_display_form` was written and
  **passed vacuously** — `"$25"` is a substring of the rendered `"$25.00"` — so
  the assertion was tightened to terminate on the ` •` separator, after which it
  failed for the right reason alongside the exact-string check (`left: "25"`,
  `right: "25.00"`). Worth recording: a green test that cannot distinguish the
  two states is not evidence, and only tightening it exposed that.
- **Wire format, green**: `cargo test` → **890 passed**, 0 failed.
- **Compile catch worth recording**: the first implementation moved `name` out of
  `ps.product` before calling `sale_price_display()`, which borrows it —
  `error[E0382]: borrow of partially moved value`. The display strings are now
  computed before the fields move.
- **T3 unblocked**: `tailwindcss` v4.3.3 is installed at
  `~/.local/bin/tailwindcss`, so `scripts/build-css.sh` can rebuild
  `static/tailwind.css` after the `@source` line is added.
