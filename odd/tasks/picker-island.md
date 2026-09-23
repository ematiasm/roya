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
- [x] T3 — The island module and the **sale** page cutover: `static/picker.js`
      (state, debounced fetch, render, derived focus and status, submit via
      `requestSubmit`), the Tailwind `@source` line and rebuilt stylesheet, the
      `line_picker` shell rewrite, the `sale_detail.html` call sites, and the e2e
      test. Vertical slice, red first. Closed in `db57ecf`.
- [x] T4 — The **purchase** page cutover: cut the entry row in
      `purchase_detail.html` over to the island shell, delete the `base.html`
      focus and in-flight handlers, rewrite the structural Rust tests in
      `purchases_web.rs` that assert the smuggled `hx-vals`, and correct the
      `static/picker.js` `htmx:load` comment to cover the purchase case.
      Closed in `ef4d194`.
      **Rescoped twice.** First: the sale page's structural tests (`sales_web.rs`
      `n4_sale_record_offers_the_picker_instead_of_the_catalogue_select` and the
      sale half of `smoke_tests.rs` `line_picker_loads_a_sale_without_a_click`)
      were originally listed here, which was wrong — they pin the sale page's
      markup, so they broke in T3 and were rewritten there. Second: the original
      wording said to **drop** the `purchase.html` Escape stand-down, and that is
      wrong too. The stand-down exists so Escape inside the picker clears the
      field **without** also closing the record menu; `base.html`'s keydown
      handler is what clears the field now, and removing the stand-down would let
      both handlers act and change the behaviour. The stand-down stays; only its
      prose ("the picker input owns Escape") is stale and gets corrected.
      `assert_entry_row_is_empty_and_focused` is expected to need **no** change —
      it asserts one `line-picker`, no `hx-swap-oob` on the row tag, `autofocus`,
      an empty value and the qty/cost ids, all of which the island shell
      preserves, so it becomes the pin that the entry-row contract survived.
- [x] T4b — Move the search coverage onto the route that survives. Closed in
      `3227470`. The HTML route has **nine consumers**, not the two the earlier
      note assumed, and they do not share one fate: some cover behaviour that
      moved to the island and is already proven in e2e, one is a shared fixture,
      and one needed retargeting to the JSON route. Dispositions, enumerated so
      no coverage was dropped silently:
      - **Delete** the two `GuardedPage` entries for the product-search fragment
        in `seed_wiring_fixture`, and the product-search part of
        `fragment_external_selectors_resolve_on_their_host_record_pages`. The
        guard mechanism and its other entries stay; the `id=` assertions on both
        host pages stay, because `#line-picker` still exists on both.
      - **Delete** `product_search_shows_the_context_price` — the price context is
        now client-side (`data-price-kind`), and e2e's
        `test_the_picker_island_owns_the_purchase_search` and its sale sibling
        already prove it.
      - **Delete** `product_search_results_announce_a_polite_match_count` — the
        count is now derived in the island, and e2e's
        `test_the_results_announce_the_match_count` already proves it.
      - **Retarget** the product-search part of `search_matches_ignore_accents_and_case`
        to `/web/product-search.json` (the matching is server-side and survives);
        its customer and supplier parts stay. Add the equivalent accent/case
        assertion to the `n5_*` JSON tests, so the surviving route pins it in its
        own module rather than only through a smoke test.
      - **Delete** the route-fragment halves of `line_picker_loads_a_sale_without_a_click`
        and `purchase_line_picker_adds_lines_without_a_click`. Their page halves
        are already the island pins.
      The HTML route itself **stays** in T4b, so this is a coverage move and the
      tree stays green.
- [ ] T4c — Delete the now-dead server path: `web_product_search`, the
      `/web/product-search` route registration, `ProductSearchResultsPartial`,
      the `line_action`/`line_target`/`price` fields of `ProductSearchQuery`, the
      route-rendered results body at the bottom of `product_search_results.html`
      (everything after `{% endmacro %}`), the `n4_product_search_*` tests that
      pinned it, and the four stale comments that describe the product search's
      `line_action`/`line_target` shape (three in `src/routes/suppliers_web.rs`,
      one in `templates/partials/supplier_search_results.html` — the supplier
      picker has its own query context now that the product search has none).
      Genuinely pure deletion, only after T4b has moved the coverage that
      survives.
- [ ] T5 — e2e sweep (`test_picker.py`, `test_search_ux.py`) and README.

## Delivery strategy

Three chained slices, because the full change touches ~10 files including two
large ones and the review workload guard applies:

- **Slice 1 — T2.** Pure addition: a new JSON route and its tests. Nothing is
  removed, nothing on screen changes, independently mergeable and revertible.
- **Slice 2 — T3.** The sale page cut over. The picker still exists in its old
  form for purchases, so the two consumers are independent.
- **Slice 3 — T4 + T4b + T4c + T5.** The purchase cutover (a behaviour change),
  then the coverage move onto the surviving route, then the deletion of the dead
  path (reviewed separately because a deletion-only diff is the cheapest thing to
  check), then the e2e sweep and docs.

The order inside slice 3 matters: **move the coverage before deleting the path.**
Deleting first would mean rewriting tests against a route that is about to
disappear, and it would make it impossible to tell a coverage move from a
coverage loss inside the same diff.

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
- 2026-09-24: the wire format refined (commit `refactor(picker): send the display
  form of money on the wire`). Money now travels in display form (decision 4).
- 2026-09-24: T3 closed in `db57ecf`. Slice 2 complete. Two findings worth
  recording: the macro signature needed **no change** (every argument is still
  used, `price_kind` now also as `data-price-kind`), so the instruction to update
  the two `sale_detail.html` call sites was a no-op in fact; and my T4 scoping
  was wrong, because the sale page's structural tests break in T3, not T4 (see
  the rescope note on T4).
- 2026-09-24: T4 in flight. A third rescope, found by reading `purchase.html`
  before writing the brief rather than after: the plan said to drop the Escape
  stand-down, and reading it showed the stand-down is what keeps Escape in the
  picker from also closing the record menu. Recorded because the pattern is
  worth keeping — **the plan's own instructions get checked against the code
  before they are delegated**, and two of the three corrections in this feature
  came from that check rather than from a failing test.
- 2026-09-24: T4 closed in `ef4d194`. Both record pages are on the island. The
  third and last rescope surfaced here, and it is the one worth generalising —
  see "Lesson: scope shared test files per half" below.
- 2026-09-24: T4b closed in `3227470`. The plan called this "pure deletion" and
  that was wrong in kind, not just in size: it is a coverage move with judgment
  in it, because nine consumers use the HTML route and only some of them cover
  behaviour that is now dead. The order was inverted — move the coverage first,
  delete the path second — and T4c was split out for the deletion alone.

## Lesson: a vacuous test is settled by mutation, not by argument

This feature produced **three** tests that were green while unable to distinguish
the correct state from the broken one:

1. `n5_product_search_json_money_is_the_fragments_display_form` passed because
   `"$25"` is a substring of the rendered `"$25.00"`. Caught by the author
   re-reading the assertion and terminating it on the ` •` separator.
2. `n5_product_search_json_folds_accents_and_case` passed because the seeded SKUs
   echoed the query text (`CAFE-1` for `"Café"`) and `match_catalogue` matches
   **name OR sku**, so every case matched through the SKU branch. The written
   non-vacuity argument was confident and wrong: it reasoned about the name and
   ignored the other half of the matcher. Caught by the orchestrator reading the
   seeded data against the matcher — not by a test.
3. The same test's inverted case: an accented query (`cafÉ` → `Café`) passes even
   without folding, so only the unaccented query can carry the gate. Recorded
   because "add an accent test" is the obvious move and it is the wrong one on
   its own.

The reusable rule: **the only thing that settles non-vacuity is running the
mutation.** For case 2 the fix (opaque SKUs `P-001`/`P-002`) makes the name branch
the only path to a match, and then breaking `normalize_search` to a plain
`to_lowercase()` makes the test fail with `products: []` for `q=CAFE`. That
mutation was run twice — once by the writer and once independently by the
orchestrator — in both cases with `src/models.rs` restored and verified
byte-identical afterwards. An argument about why a test cannot be vacuous is a
hypothesis; the mutation is the evidence.

Corollary for briefs: when a test asserts that a **substring** matched or a
**search** found something, name the alternate match paths in the brief (a second
field, a second branch, a formatted variant) and require the test to close them.
And when the mutation needs a file outside the writer's surfaces, the orchestrator
runs it rather than accepting the argument.

## Lesson: scope shared test files per half, not per file

Three rescopes in this feature, all the same shape: I listed a **file** in a task
when the thing that breaks is a **half of a test inside that file**, and the two
halves belong to different pages, so they break in different tasks.

- `src/smoke_tests.rs` holds `line_picker_loads_a_sale_without_a_click` (sale
  half breaks in T3) **and** `purchase_line_picker_adds_lines_without_a_click`
  (purchase half breaks in T4). Both times the file was out of scope when its
  half broke.
- `src/routes/sales_web.rs` and `src/routes/purchases_web.rs` are per-page, so
  they were scoped correctly by accident.
- Worse, both smoke tests are **multi-part**: each has a route-fragment half
  asserting the `/web/product-search` response body and a record-page half
  asserting the page markup. The route-fragment half is shared by both pages and
  must stay byte-identical until T4b, so a rewrite scoped "to the file" or even
  "to the test" would have destroyed the pin that the purchase-facing route body
  was undisturbed.

The generalisation, for the next feature: **when a task changes a page's markup,
grep the whole test tree for that page's transport attributes and enumerate each
matching assertion with its enclosing test name, not just the file.** A file that
covers two pages is two surfaces. And when a test has parts, name the part in the
scope — "rewrite the record-page half of X" — never the file or the test.

## Accepted debt

- **The failed-search notice is an event re-dispatch.** The island has no notice
  region of its own, so on failure it dispatches an `htmx:responseError`-shaped
  `CustomEvent` for `base.html`'s existing notice handler to consume — one notice
  contract, two transports. Accepted because the alternative is a second notice
  authority, which the repo's own comment forbids ("Change the two together,
  never one without the other"). Revisit if htmx's event shape changes, and note
  that the notice names the raw request path (`/web/product-search.json`) because
  the picker input carries no `data-action`; that wart predates the island (the
  old path was `/web/product-search`) and is not a regression. The refinement was caught by asking what the island would
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
- **T3, red first**: the new e2e test failed with `Locator expected not to have
  attribute — unexpected value "/web/product-search"`, because the field still
  carried the old declarative read and the island did not exist yet.
- **T3, green (orchestrator-verified, not taken from the worker's report)**:
  `cargo test` → **890 passed, 0 failed**; `scripts/e2e.sh` → **92 passed,
  4 skipped, 0 failed**, the four skips being pre-existing opt-in probes
  confirmed by their own skip reasons.
- **T3, Tailwind purge assertion**: `select-none` appears only in
  `static/picker.js` (`grep -rn "select-none" templates/` → no matches) and
  survives the rebuild into `static/tailwind.css`.
- **T3, the two structural rewrites**: verified by reading them, not by trusting
  the report — both pinned the removed declarative transport
  (`hx-get`/`delay:`/`hx-on:keyup`) and were rewritten to pin the island's
  contract (`data-picker`, `data-price-kind="sale"`, no search transport on the
  field, hidden `product_id`, results container still a form sibling, default
  qty). The route-fragment half of the smoke test was confirmed byte-identical by
  diff, so it now doubles as the pin that the purchase-facing body is untouched.
- **T3, a defect the worker did not report and the orchestrator found**: the
  change removed the field's `hx-on:keyup` Escape clearing, which made
  `base.html:228` lie about the mechanism. Fixed in the same commit; the comment
  now describes the real handoff and stays accurate for the purchase mirror.
- **T3, a design correction the orchestrator required**: the island originally
  remounted after the OOB swap through a whole-page `MutationObserver` with
  `subtree: true`, which the island's own `render()` fed on every keystroke. It
  now rides `htmx:load`, with the vendored htmx 1.9.12 source read to confirm it
  fires for OOB content rather than assumed.
- **T4, red first**: the new e2e test failed with the field still carrying
  `hx-get=/web/product-search`, exactly the transport the change removes.
- **T4, green (orchestrator-verified)**: `cargo test` → **890 passed, 0 failed**;
  `scripts/e2e.sh` → **93 passed, 4 skipped, 0 failed** (the +1 over T3 is the new
  purchase island test).
- **T4, by inspection**: `base.html` has zero occurrences of
  `pendingResultFocus`, `searchFailed`, and the picker `beforeRequest`/
  beforeSwap/afterSwap target checks, while the Escape-clearing keydown handler
  is intact; `purchase_detail.html` carries `data-picker` and
  `data-price-kind="cost"` and the hidden `product_id`, with no transport on the
  field and no `hx-swap-oob` on the row (the only two `hx-swap-oob` in that file
  belong to the action bar); the Escape stand-down is still at
  `purchase.html:99`; and the `static/picker.js` diff is comment-only, confirmed
  by grepping the diff for class changes (none), so the committed stylesheet is
  still current.
- **T4, a helper that survived**: `assert_entry_row_is_empty_and_focused` needed
  no change and passes, which makes it the pin that the entry-row contract came
  through the cutover intact. Predicting this before delegating was the payoff of
  reading the test first.
- **T4, a brief that was wrong in the worker's favour**: the brief told the
  worker to update the purchase parts of `e2e/tests/test_search_ux.py`, which has
  none — every picker test in it runs against the sale page. The worker said so
  instead of inventing work.
- **T4b, green (orchestrator-verified)**: `cargo test` → **889 passed, 0 failed**.
  The arithmetic is exact and was checked against the plan: 890 before, − 2 whole
  tests deleted, + 1 new gate = 889. The instruction given to the worker omitted
  the +1, which the worker caught rather than silently matching the number.
- **T4b, the vacuous gate, caught and settled**: the new
  `n5_product_search_json_folds_accents_and_case` seeded SKUs that echoed the
  query text, and `match_catalogue` matches **name OR sku**, so all four cases
  matched through the SKU branch. The orchestrator found it by reading the seeded
  data against the matcher; the written non-vacuity argument in the report had
  reasoned about the name only. Fixed with opaque SKUs, then **proven by mutation
  run twice** — writer and orchestrator independently — breaking
  `normalize_search` to a plain `to_lowercase()` makes the test fail with
  `{"products":[],"query":"CAFE"}`. `src/models.rs` was restored and verified
  byte-identical to HEAD afterwards (`git diff` and `git status` both empty).
- **T4b, kept invariants verified**: the four island-pin markers in the
  record-page halves, the two host-page `id=` assertion loops, and the customer
  and supplier parts of `search_matches_ignore_accents_and_case` (18 seed calls)
  are all still present; the two deleted tests are gone; and the HTML route
  handler plus its three `n4_product_search_*` tests are untouched, as T4c needs
  them to still be there.
- **T4b, auditability**: each deleted test is replaced by a one-line comment at
  its former site naming the e2e test that now carries the coverage, so the
  deletion is greppable rather than silent.
