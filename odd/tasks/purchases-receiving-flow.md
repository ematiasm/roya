# Feature: Purchases receiving flow — the swap bug, auto-saved identity, and the entry island

## Objective

Make the purchase receiving flow do what it looks like it does: fix a live defect
that corrupts the page on a line edit, make the document's identity save itself
so the operator stops being asked to press a button for something that already
saves, and give the entry row one owner for its client state.

## Problem

Read from the tree on 2026-09-24, after `picker-island` merged as PR #94.

**1. Editing a line's quantity or cost duplicates the whole record body inside
the money region.** Measured, not deduced, with a temporary Playwright
diagnostic on a draft purchase:

```
BEFORE  #purchase-header: 1  #purchase-header-form: 1  #purchase-record-inner: 1  #purchase-record-money: 1
AFTER   #purchase-header: 2  #purchase-header-form: 2  #purchase-record-inner: 2  #purchase-record-money: 2
```

Both new headers report `parent=purchase-record-inner`, so the second copy is
nested inside the money region.

The mechanism is a swap-contract mismatch. `changed_with_notice` returns the
**whole record body**, and the two consumers disagree about how to consume it:

| Element | `hx-swap` | `hx-select` | Result |
|---|---|---|---|
| add-line form (`purchase_detail.html:53`) | `outerHTML` | `#purchase-record-money` | correct — only the region is taken |
| inline qty edit (`:122`) | `innerHTML` | **absent** | the whole body becomes the region's `innerHTML` |
| inline cost edit (`:131`) | `innerHTML` | **absent** | same |

The consequences are not cosmetic. The duplicate ids make every id-based lookup
ambiguous: `hx-include="#line-qty-{id}, #line-cost-{id}"` now matches two
elements, `hx-target` resolves to whichever comes first in the document, and the
picker island's `mount()` would mount the duplicated `[data-picker]`. **The page
is corrupt until it is reloaded.** This is the same family as the hidden-id
defect the picker work retired: an id-based lookup silently resolving to the
wrong element.

Attribution: pre-existing, introduced by `b29a55c feat(purchases): edit purchase
lines inline via PUT` (2026-09-22) — the T7 task of
`odd/tasks/purchases-receiving-desk.md`. Verified present at `d411d4e~1`, and
the `picker-island` merge only **removed** an `innerHTML` swap (the picker's old
search transport) without adding any. Blast radius: **two inputs, purchase page
only** — the sale page has no inline line edit (`hx-put` does not appear in
`sale_detail.html`).

**2. `Save header` is a button for something that mostly saves itself.** The
supplier picker's post already carries the whole header through `hx-include`, so
the identity fields save on their own when the operator touches the supplier. The
three other fields (`#record-purchase-date`, `#record-invoice-no`,
`#record-notes`) have **no `hx-trigger` at all** — they are plain inputs, so
changing them alone saves nothing until the button is pressed. The button is not
redundant today; it is the only way to save those three. It reads as redundant
because half the header really does auto-save, which is worse than either
extreme.

**3. The entry row's client state has no owner.** Seven pieces of state across
four owners — the DOM, `purchase.html`'s `defaultValue` rollback, the Askama
fragment, and the response. This is the same defect class the picker work
retired, one widget over, and it is why the inline line edit needs hand-written
rollback glue.

## Why

The first item is a correctness defect and is the reason this feature exists. The
second and third are the flow friction the operator reported, and they share a
root cause with the first: a region that several controls swap into, with no
single statement of what a response contains or who consumes it.

## Scope

- `templates/partials/purchase_detail.html` — the two inline edit inputs, the
  header form, the entry row
- `templates/partials/purchase.html` — the page-shell script (`defaultValue`
  rollback, dialog/menu handlers)
- `src/routes/purchases_web.rs` — the header route's answer, `render_record`'s
  callers
- `static/picker.js` — untouched; the entry row island is a new asset
- New: `static/entry-row.js`
- `assets/tailwind.css` + rebuilt `static/tailwind.css` (the new island is a
  Tailwind scan source)
- Rust tests in `src/routes/purchases_web.rs`, and the e2e purchase suites
- `README.md`

Out of scope: the sale page (no inline edit, so no instance of the bug), the
`#purchase-action-bar` OOB contract, the payments table, and the drawer state
machine duplicated across five templates.

## Constraints

- **No framework, no build step** for the island — the same posture as
  `static/picker.js`.
- **The header route's server contract changes** in T2 (it stops answering with
  the record body), so T2 must state the new contract explicitly rather than
  leaving it implicit in an attribute.
- **Keep the page order.** The user explicitly chose to keep the current order
  (identity above, lines below) and only remove the save button.
- **Tailwind `source(none)`**: a new JS asset must be added as an `@source` or
  its classes are purged. `assets/tailwind.css` currently declares
  `../templates` and `../static/picker.js`.

## Locked design decisions

**T1 — the swap fix.**

1. Both attributes change together:
   `hx-swap="outerHTML" hx-select="#purchase-record-money"`, mirroring the
   add-line form. **Adding only `hx-select` while keeping `innerHTML` would nest
   `#purchase-record-money` inside itself and keep the duplicate id**, so the two
   are one decision, not two.
2. The fix is pinned by a regression test that asserts the **element counts are
   unchanged** across an inline edit, not by asserting the attribute. The
   attribute could be right while the behaviour is wrong; the counts cannot.

**T2 — auto-saved identity.**

3. `hx-trigger="change"` (blur), never `input` — otherwise every keystroke posts.
4. **The response does not swap the record.** It answers with an empty body and
   the `HX-Trigger` the page already listens for, and the UI shows a subtle
   "Saved" indicator. This is what keeps auto-save from stealing focus: a
   full-record swap on every blur would replace the field the operator is
   tabbing into. If the audit line (`Updated by …`) must not go stale, it is the
   only thing that rides an OOB swap.
5. `hx-sync` serializes the field posts so an out-of-order response cannot clobber
   a newer value.
6. **Nothing is posted while a field is incomplete or invalid.** The date is
   `required`; clearing it to retype must not fire a save that answers 400.
7. Field errors render inline next to the field, not in the global notice region:
   with one submit per blur there is no single moment where a global notice makes
   sense.
8. The `Save header` button is retired, and the picker's `hx-include` path keeps
   working unchanged — it already saves the whole header and must not regress.
9. The page order does not change.

**T3 — the entry row island.**

10. Same shape as `picker-island`: whole widget, one state object, one `render()`,
    plain JS, no dependency, derived focus and derived status.
11. The island owns the add-line submit, so `product_id` stops being
    markup-owned state. The hidden input stays `disabled` while nothing is
    selected, the mechanism that retired the hidden-id defect.
12. The server's add-line contract does not change: htmx keeps the swap, the OOB
    fragments and the `HX-Trigger` events.

## Acceptance criteria

- [ ] An inline line edit leaves the element counts unchanged: one
      `#purchase-record-money`, one `#purchase-record-inner`, one
      `#purchase-header`, one `#purchase-header-form`.
- [ ] The regression test fails before the fix and passes after, for the right
      reason (counts, not attributes).
- [ ] Changing the date, invoice or notes alone persists without pressing
      anything, and the field keeps focus while doing it.
- [ ] `Save header` no longer exists on the page.
- [ ] The supplier picker still saves the whole header (no regression).
- [ ] An incomplete or invalid field posts nothing and reports inline.
- [ ] The entry row has one state object; no entry-row state lives in
      `purchase.html`.
- [ ] `cargo test` green; `scripts/e2e.sh` green.
- [ ] README documents the new asset and its Tailwind scan-source status.

## Applicable checks

- `cargo test` (primary; `openspec/config.yaml` sets `strict_tdd: true`)
- `cargo test purchases_web` while iterating
- `cargo check --all-targets`
- `scripts/e2e.sh` and `scripts/e2e.sh -k purchases` / `-k picker`
- `scripts/build-css.sh` plus a purge assertion for the new island's classes

## TDD

`strict_tdd: true`. T1's red check is the count regression test, which is already
proven to fail: the diagnostic above was written, run, and produced the duplicate
counts before any fix existed. T2 and T3 write their failing browser checks first.

## Tasks

- [x] T1 — **Fix the duplicate-region defect.** Change the two inline edit inputs
      (`purchase_detail.html:122`, `:131`) to
      `hx-swap="outerHTML" hx-select="#purchase-record-money"`, and add the count
      regression test to `e2e/tests/test_purchases.py` first. Independently
      shippable: one file, one test, no design decision. Closed in `f3f7962`.
- [x] T2 — **Auto-saved identity.** Closed in `e0311b6`. The header form posts
      on `change` with `hx-swap="none"` and `hx-sync="this:replace"`, the
      `Save header` button is deleted, and a two-second `aria-live` indicator
      replaces it. **Two refinements to the approved design, both recorded here
      because they change the contract:**
      - **The header route's answer did NOT change.** The plan said to make it an
        empty body plus `HX-Trigger`. Instead the form stops swapping it
        (`hx-swap="none"`), which leaves the route's contract alone and keeps the
        supplier picker's consumer of the same route untouched. Smaller change,
        same outcome.
      - **Inline field errors are client-side only.** htmx does not swap 4xx
        responses, so rendering a *server* failure inline would mean swallowing
        the status and answering 200 — worse than the problem. The case that
        actually mattered, the empty required date, is client-side and is
        covered. Server failures keep the global notice, which is what they do
        today.
- [x] T3 — **The entry row island → DROPPED, and replaced by what the
      measurement found.** Closed in `181c7a7`. The task as written was wrong in
      two ways, and both were settled by measuring instead of refactoring:
      - **The entry row is already an island.** `picker-island` T4 cut it over in
        this same line of work: `#line-picker` carries `data-picker` and
        `data-price-kind="cost"`, and `static/picker.js` owns its query, matches,
        focus, selected product and submit. The task was written from an
        inventory that predated the picker island and never re-checked.
      - **The inline line edit — the real remaining candidate — holds up.** The
        swap replaces the whole money region, so focus loss and stale totals were
        both plausible. Both measurements refuted it: htmx restores focus for
        elements carrying an id (measured `active='line-cost-1'` before and
        after), and the derived numbers follow (`$12.00`→`$30.00`,
        `$18.00`→`$36.00`). The rollback glue is reachable too: htmx issues the
        PUT for an HTML-invalid quantity despite the `min`, the route refuses,
        `elt.value = elt.defaultValue` reverts the field, and nothing is stored.
      What is left is style, not behaviour — eight lines of glue and two inputs
      duplicating their `hx-put`/`hx-include` — so the island is dropped rather
      than performed. **What was kept is the coverage**: these three behaviours
      had no browser test at all, pinned only by markup assertions, which is the
      same gap that let the duplicate-region defect live two days. Two tests now
      drive them, and each asserts that the thing it guards actually happened.

## Delivery strategy

Three slices, in the order the user chose, each independently revertible:

- **Slice 1 — T1.** A defect fix with its regression test. Ship it alone and
  first: the page is corrupt today and the fix carries no design decision.
- **Slice 2 — T2.** The identity auto-save and the retired button.
- **Slice 3 — T3 (dropped).** The entry row island was already done by
  `picker-island`, and the inline line edit it was standing in for measured clean.
  What shipped instead is the browser coverage those behaviours never had.

## Progress

- 2026-09-24: document created. The defect was reproduced with a temporary
  Playwright diagnostic, the mechanism traced to the missing `hx-select` on the
  two inline edit inputs, and the attribution established (`b29a55c`, not the
  `picker-island` merge). Design agreed with the user: keep the page order,
  auto-save the identity, and answer without swapping the record so focus
  survives. The diagnostic was deleted after reading its output; its assertion
  becomes T1's regression test.
- 2026-09-24: T1 closed in `f3f7962`. Slice 1 complete and independently
  revertible. The defect is fixed on `main`'s next merge and the three consumers
  of the money region now agree on the same swap contract.

**Accepted debt, recorded so it is not lost**

- **The audit line (`Updated by …`) is stale until the next full load.** With
  `hx-swap="none"` nothing re-renders it. The fix is a small OOB refresh, and it
  is deliberately not in T2: it would stack a second unverified htmx behaviour
  (`hx-swap="none"` together with `hx-swap-oob`) into a change that already had
  a collision to handle. The vendored source was read and the OOB scan does run
  before the swap regardless of style, so the mechanism should work — it just
  deserves its own verification rather than being assumed here.
- **Server-side failures render in the global notice, not inline.** See the T2
  refinement above for the reason (a 4xx must stay a 4xx).

## Verification evidence

- **T2, the collision the plan and the design review both missed**:
  `base.html`'s `htmx:afterRequest` success handler announces
  `"<action> saved"` for **every** successful form post carrying a
  `data-action`. Auto-save would have raised one notice per field. Found by
  reading the handler before writing the brief, not by a failing test. The form
  now opts out with `data-silent-save`, placed after the `data-notice-server`
  tiebreak and before the notice, and `data-action` stays so the failure path
  still names the action.
- **T2, the focus test passed vacuously at first.** With no auto-save yet
  implemented, nothing swapped and focus survived trivially, so the test of the
  feature's central decision proved nothing. It now also asserts that the save
  happened, which is what makes the focus check a real gate — the red message
  says so: `the save did not happen, so the focus check is vacuous`. Fourth
  instance of the vacuous-test pattern in this session, and the first caught
  before the test was accepted.
- **T2, red first**: 4 failed, each for the right reason — the button still
  present, the save not happening, no response to wait for, and no inline hint.
- **T2, green (orchestrator-verified)**: `cargo test` → **885 passed,
  0 failed**; `scripts/e2e.sh` → **98 passed, 4 skipped, 0 failed** (94 plus the
  four new tests).
- **T2, two tests pinned retired mechanisms and were corrected**: the e2e header
  test clicked the deleted button **and** its docstring claimed the record was
  swapped, which stopped being true; and a Rust read-only assertion matched the
  bare substring `purchase-header-form`, which the new page-shell selector string
  now trips — corrected to `id="purchase-header-form"`, still a negative
  assertion and now more precise rather than weaker. **The e2e one was my error
  and a worse kind than a scope miss**: I had already grepped `purchase-header`
  across the e2e suite and seen that test clicking the button, and did not put it
  in the brief.
- **T2, htmx verified against the vendored source rather than assumed**:
  `hx-trigger="change"` on the form element binds the listener on the element
  itself with no target filter, so a bubbling `change` from a field fires it; and
  `hx-sync` strategy `replace` dispatches `htmx:abort` on the sync element, so
  "the newer request wins" is its literal semantics.
- **T2, no stylesheet rebuild needed**: every class the change adds
  (`mt-1`, `text-[13px]`, `text-danger`, `text-muted`) already exists in
  `templates/`, and the new script adds no styling class, so the committed
  `static/tailwind.css` stays current. Verified rather than assumed.
- **T3, the task was dropped because the measurement said so, and the first
  measurement refuted its own hypothesis.** The inline edit swaps the whole
  money region with `outerHTML`, so the focused field being destroyed looked
  certain. It is not: `active='line-cost-1'` before and after the swap, because
  htmx restores focus for elements carrying an id. **Inference from markup has
  now been wrong twice in this feature** (the focus loss here, and the OOB
  question in T2) and right zero times; the measurements were right both times.
- **T3, the rollback glue is reachable, which I also got wrong by inference.**
  Both inputs carry HTML constraints (`min="0.01"`, `min="0"`), so the values
  the domain refuses looked unreachable from the browser. htmx issues the PUT
  anyway: two PUTs for `-5` and `0`, the route refuses, the field reverts to
  `2`, the stored quantity stays `2`.
- **T3, green (orchestrator-verified)**: `cargo test` → **885 passed,
  0 failed**; `scripts/e2e.sh` → **100 passed, 4 skipped, 0 failed** (98 plus
  the two new tests).
- **T3, a test bug worth recording**: the first version of the rollback
  measurement tried `"abc"` as an invalid quantity and failed with
  `Cannot type text into input[type=number]` — a defect in the test, not the
  app. Removed the case rather than working around it; a number input cannot
  carry text and pretending otherwise would have been a test that lies.

## Outcome

The operator-visible result: the identity fields save themselves and the `Save
header` button is gone; a line edit no longer duplicates the page; and three
behaviours of the lines table are now covered by a browser instead of by markup
assertions.

What was deliberately **not** built: the entry-row island, because it already
existed, and the inline-edit island, because measuring showed there is no
behaviour left to own. Both are recorded above with the measurements that settled
them, so the next person does not re-open the same question from the same stale
inventory.

What was deleted: the second search transport (T1's slice), the `Save header`
button, the generic success notice for that one form, and the `innerHTML` swap
contract that duplicated the record body.

What was kept on purpose: the keydown and Escape handling and the single notice
authority in `base.html`, the rollback glue in `purchase.html`, and the page
order the user chose.

- **T1, the coverage gap that let it live two days**: the inline line edit had
  **no browser test at all**. The only thing mentioning it was a Rust test
  asserting the input exists in the markup
  (`src/routes/purchases_web.rs:3918`). The markup was right, the behaviour was
  broken, and nothing looked at the behaviour — the same markup-pin-instead-of-
  behaviour pattern this codebase keeps producing. The new test is the first
  browser test of the inline edit.
- **T1, red first, and the first red was the wrong red**: the test's initial run
  failed with `SeedError: tracked products require min_stock and max_stock`, not
  with the duplicate. Fixed the seed before reading anything into it, then the
  real red arrived: `before={'#purchase-record-inner': 1, '#purchase-header': 1,
  '#purchase-header-form': 1, '#purchase-record-money': 1}` vs `after={...: 2}`.
  Recorded because a red for the wrong reason is not evidence either.
- **T1, the mechanism closed by the `#line-picker` count**: it stayed at 1 while
  the other four doubled. That is consistent, not anomalous — `innerHTML` wipes
  the target's children, so the original `#line-picker` is removed and only the
  one inside the inserted body remains. The four that doubled are the target
  itself and its ancestors.
- **T1, green (orchestrator-verified)**: the named test **1 passed**;
  `cargo test` → **885 passed, 0 failed**; `scripts/e2e.sh` → **94 passed,
  4 skipped, 0 failed** (93 plus the new test).
- **T1, by inspection**: the diff is the two coupled attributes on both inputs
  plus the comment — `+10 / -3`, nothing else; and the grep for
  `hx-target="#purchase-record-money"` combined with `hx-swap="innerHTML"` is
  empty, so all three consumers (add-line at `:53`, qty at `:127`, cost at
  `:137`) now carry the same `outerHTML` + `hx-select` contract.
