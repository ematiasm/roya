# Drawer — one reusable component, split on wide screens

## Status

**Delivered.** All seven tasks landed; the full suite is green
(`128 passed, 4 skipped`, and `cargo test` 1223).

| task | commit |
| --- | --- |
| T1 tokens + `.drawer` component | `38f1860` |
| T2 the shared controller | `50cb4bf` |
| T3 migrate the five modules | `e890813` |
| T4 the cap and the one unwrapped table | `9adad56` |
| T5 the rail empty state | `65e8fc8` |
| T6 the layout spec, mutation-proven | `00d2b21` |
| T7 regenerate and audit the baseline | `e4ae700` |

Three defects were found by measuring rather than by reading, and all three
would have shipped: a `@media` condition using `var()` that parsed, built with
exit 0, and never matched anything; a `max-w-7xl` that is 1280px and not the
1536px the plan claimed; and a `var el` shadowing the IIFE's own DOM builder.
One piece of code was also found to be unobservable — the controller's rail
guard, which the stylesheet already guarantees — and the comment that credited
it was corrected instead of the branch being deleted.


## Objective

One drawer component that all five modules share, whose width adapts to the
available viewport, and which stops overlaying the list on a wide screen and
becomes a permanent side panel instead.

The user reported the drawer and its list still render phone-sized on a
full-screen PC. Verified cause: every one of the five shells is hard-capped at
`max-w-md` (448px) at every viewport, and the overlay is anchored to the
viewport's `right-0` while the content column lives inside a `max-w-[1080px]`
cap — so at 1920px the overlay is misaligned with the content by 292px of dead
gutter and covers only 156px of real content.

## Decisions, settled

1. **Split, not a wider overlay.** An overlay and the list compete for the same
   width, always; one of them can only win. A side panel is in flow, so the list
   and the detail can both be wide. At 1920px: panel 560px (wider than today's
   448px) and list 888px (near today's 1016px). Both improve, which is the only
   argument that settles it.
2. **Split threshold `90rem` (1440px).** Below it the panel stays an overlay.
   1440 − 256 (sidebar) − 64 (padding) − 24 (gap) − 448 (panel) = **648px of
   list**, the floor at which the list is still usable. 1280 leaves 536 (tight),
   1024 leaves 320 (unusable). 1440 is where the arithmetic closes.
3. **This threshold is also a test-safety decision, not only a UX one.** The
   default Playwright viewport in this repo is 1280×720, already above `lg`.
   Splitting at `lg` would silently invert ~25 e2e assertions
   (`not_to_be_visible()`, "Escape empties the body") into vacuous passes, and
   only ONE test in the repo runs below 1024px. Splitting at 1440 keeps the
   default viewport in overlay mode, so the suite stays correct by construction
   and the overlay path keeps real coverage.
4. **The 1080px cap on `main` must rise.** It is the binding constraint, not the
   viewport: at 1920px extra screen width currently buys nothing. `max-w-7xl`
   (1536px) — a named token, equal to the `2xl` breakpoint, leaving a 64px
   gutter at 1920px so the column never touches the sidebar. It appears in
   exactly one place (`base.html:39`) and no test asserts it.
5. **One component class + one controller; NO shared partial yet.** The partial
   is the tempting move and the wrong first one: the products header carries an
   "All products" link the other four lack, and customers opens the panel from
   the server. A partial starts with two exceptions in one shared file. The
   component plus the controller give the reuse with zero exception surface.
   Extract the partial when a third module needs the same exception.
6. **Width adapts by available width, never by device.** `clamp()` and media
   queries, no `userAgent`, no JavaScript detection. Layout depends on the space
   available, not on a device class — a half-screen window on a 13" laptop has
   less width than an iPad in landscape.

## The geometry, computed

Tailwind v4 defaults apply (no config file): `md` 768, `lg` 1024, `xl` 1280,
`2xl` 1536. `max-w-md` = 448px, `lg:w-64` = 256px, `gap-6` = 24px.

| viewport | overlay panel | list | dead gutter | rail panel | list | dead gutter |
| --- | --- | --- | --- | --- | --- | --- |
| 360 | 360 (full bleed) | 328 | 0 | 360 | 328 | 0 |
| 768 | 400 (clamp floor) | 672 | 0 | 400 | 672 | 0 |
| 1024 | 430 | 704 | 0 | 430 | 704 | 0 |
| 1280 | 538 | 960 | 0 | 538 | 960 | 0 |
| **1440** | 605 | 1016 | 52 | **448 (rail)** | **648** | 0 |
| 1600 | 640 (clamp cap) | 1016 | 132 | 448 (rail) | 808 | 0 |
| **1920** | 640 | 1016 | **292** | **560 (rail)** | **888** | 64 |
| 2560 | 640 | 1016 | 612 | 560 (rail) | 888 | 384 |

Below 1440 the two variants are deliberately identical, so the comparison
isolates one variable. Accepted costs: a 1280px laptop keeps the overlay (better
than today's 448px, but not the comfortable panel); and above 1440 there are two
interaction models (click opens an overlay below, the panel is already there
above), which is a real muscle-memory cost. `max-w-7xl` is a token choice, not a
law — 2560px still wastes 384px per side, and that is left as-is unless asked.

## Today: what exists, measured

| | |
| --- | --- |
| shells | 5 hand-copied divs, 11 identical utilities each |
| JS | 10 `open*Drawer`/`close*Drawer` functions, 5 inline `<script>` blocks |
| shared partials | **0** |
| `.drawer` in the stylesheet | **does not exist** |
| responsive `max-w-*` | **none** — no prefixed variant exists in the compiled CSS |

The five class strings already drifted: utility order differs in 3 of 5.

**The split precedent the repo has does NOT generalise.**
`min-[900px]:grid-cols-[minmax(0,1fr)_360px]` lives in `dashboard.html:12` and
`sales.html:11`, and on both it is a *permanently visible rail of create-forms* —
no show/hide, no per-open fetch, no close path. Only the class string is
reusable. `odd/tasks/redesign-purchases-index.md:158` claims the grid is in
`purchases.html` and is stale. That grid's `min-[900px]` also sits below `lg`, so
between 900 and 1024px `/sales` and `/` have a rail while the modules have an
overlay. Left alone here — different concept, and changing it is out of scope.

## Design

### Tokens (`@theme`, `assets/tailwind.css`)

```css
--drawer-rail: 28rem;      /* 448px — the side panel at 90rem..120rem */
--drawer-rail-lg: 35rem;   /* 560px — the side panel at and above 120rem */
--drawer-overlay-min: 25rem;
--drawer-overlay-max: 40rem;
--drawer-split-at: 90rem;
```

### Component (`@layer components`)

`.drawer` — the shell. Full bleed below 768px (the detail IS the screen on a
phone), `clamp(25rem, 42vw, 40rem)` above it, then in-flow and un-shadowed at
`--drawer-split-at`. `.drawer-split` — the two-column grid, one column below the
threshold and `minmax(0, 1fr) var(--drawer-rail)` at and above it, so the list
absorbs the remainder and can never push the panel.

The panel is `width: 100%` of its grid track and the track width is declared once
on `.drawer-split` — one source of truth, so the two can never disagree.

**Open state is `data-open`, not `hidden`.** `hidden` is a Tailwind utility and
`@layer utilities` is emitted after `@layer components`, so a `hidden` on the
element would beat the component's own `display`. With `data-open` the cascade is
unambiguous, and `display: none` still satisfies the existing
`not_to_be_visible()` idioms unchanged.

### Controller (one, in `base.html`)

Replaces all ten functions. Opt in with attributes, not per-module wiring:

- `data-drawer="<id>"` on the panel, `data-drawer-body="<id>"` on the slot.
- `htmx:afterSwap` on a `data-drawer-body` target opens the named panel — one
  listener replaces the five per-module ones.
- `data-drawer-close-on="<event> [<event>]"` binds the close events. Five
  variants collapse into one attribute.
- `data-drawer-guard="#dialog-a,#dialog-b"` makes Escape skip the panel while a
  dialog is open. **This also fixes a latent bug**: purchases and documents have
  NO dialog guard today and close the panel out from under an open `<dialog>`.
  That is a bug to fix, not behaviour to preserve.
- Escape is a single document listener that stays silent unless a panel is
  actually overlaying, and never calls `preventDefault()` on a bare Escape —
  `base.html`'s picker handler at `:239` depends on it.

Per-module **list refresh** listeners stay where they are: they are list
concerns, not drawer concerns, and their targets genuinely differ.

Preserve: `customers`' server-opened panel (`drawer_open` at
`customers_web.rs:109`, set by `customer_statement_page` at `:499` — the
statement page, not a detail route); and purchases' row being an `<a>` with a
working `href`, which two tests pin as the no-JS fallback.

### Empty state

Above the split the panel is always present, so it needs a designed empty state
("select a row") rather than a hole. New `MessageKey` in both catalogs. At or
above the split the ✕ clears the body to that state instead of collapsing the
rail — the rail is permanent, the selection is what ✕ clears.

## Tasks

- [x] **T1 — tokens + component.** `@theme` tokens and `.drawer`/`.drawer-split`
  in `assets/tailwind.css`. Rebuild `static/tailwind.css`. Guard
  `every_class_used_by_a_scanned_source_has_a_rule_in_the_committed_stylesheet`
  (`src/stylesheet_tests.rs:832`) must pass. **Touches no template.**
  **Delivered `38f1860`.** `:root` grew 39 → 43 declarations with nothing
  removed or modified; the built file compiles with **no warnings**; the
  stylesheet guard is 10 passed; `cargo test` 1223 passed; the visual baseline
  3 passed with `visual-baseline.json` **byte-identical** — which is the proof
  the addition is inert until T3.

  Measured in a real Chromium against the built file, four widths:

  | viewport | split | panel | panel width | list |
  | --- | --- | --- | --- | --- |
  | 360 | block | fixed | 360 (full bleed) | — |
  | 1280 | block | fixed | 538 | 960 |
  | 1440 | grid | static | 448 | 648 |
  | 1920 | grid | static | 560 | 1016 † |

  † inflated: the probe omits the `max-w-[1080px]` cap that T4 raises. With
  `max-w-7xl` the 1920 list is 888, as computed in the table above.

  A closed panel reads `display: none` below the threshold and `display: flex`
  at and above it, so the rail is permanent and the overlay is genuinely
  dismissible.

  **Defect found and fixed inside T1 — read this before changing the
  threshold.** The split threshold was first written as
  `@media (width >= var(--drawer-split-at))`. That is invalid: a custom
  property is not available in a media query condition, because the cascade
  resolves `var()` against an element and a media query has none. Lightning CSS
  emits it verbatim and warns "Unexpected token Function(var)", the build
  succeeds, and **the condition then matches nothing in any browser** — so the
  split would never have activated, at any width, with no console error and no
  build error. Measured in Chromium at 1920px: a rule behind
  `@media (width >= var(--x))` stayed unapplied while the identical rule behind
  `@media (width >= 90rem)` applied. The threshold is now a literal at exactly
  two sites, both commented, and `--drawer-split-at` was deleted rather than
  left in place — a token that silently does nothing is worse than no token.
  The build now reports zero warnings and no `var()` survives inside any media
  condition.

- [x] **T2 — controller.** The generic drawer controller in `base.html`, with the
  `data-*` opt-in surface and the single Escape listener. **Delivered `50cb4bf`.**
  Lands **inert** — no template carries the opt-in attributes yet, so T3
  migrates the five and deletes the ten functions it replaces. `cargo test` 1223
  passed; the four drawer e2e specs 62 passed 2 skipped (both are opt-in
  screenshot probes).

  Opt-in surface: `data-drawer` on the shell, `data-drawer-body` on the slot,
  `data-drawer-open` / `data-drawer-close` on the controls, `data-drawer-close-on`
  for the htmx events that dismiss the panel, `data-drawer-guard` for the dialogs
  that own Escape.

  The guard tests an element's **`open` property, not its presence** — a
  `<dialog>` is always in the DOM, so a presence test would guard the drawer
  forever and Escape would never close it. The guard also **fixes** the two
  modules that had none: purchases and documents used to close their panel out
  from under an open dialog.

  Closing empties the slot but collapses the panel only while it is overlaying;
  on the rail ✕ clears the selection instead, which is the empty state T5 needs.

  The sidebar's local `drawer(open)` is renamed `toggleSidebar`: "drawer" now
  names the detail panel everywhere else in the file, and one word cannot mean
  two components.

- [x] **T3 — migrate the five modules.** `products.html:193`, `purchases.html:86`,
  `customers.html:76`, `documents.html:39`, `suppliers.html:57` onto
  `.drawer` + `.drawer-split`; delete the ten functions; keep every refresh
  listener; preserve the customers server-open and the purchases anchor.
  **Delivered `e890813`.** `cargo test` 1223 passed; the four drawer e2e specs
  62 passed 2 skipped, and those specs open the modals.

  Opt-in attributes as landed:

  | module | `data-drawer-close-on` | `data-drawer-guard` |
  | --- | --- | --- |
  | products | `product-saved drawer:close` | `#new-category-dialog,#new-product-dialog` |
  | purchases | `purchase-changed` | `#new-purchase-dialog` |
  | customers | `drawer:close` | `#new-customer-dialog,#customer-edit-dialog` |
  | documents | `sale-changed purchase-changed` | *(none — it has no dialog)* |
  | suppliers | `drawer:close` | `#new-supplier-dialog,#supplier-edit-dialog` |

  ### Three things the plan did not know

  **1. Nine call sites lived outside the five shells, and the plan missed them.**
  `partials/product_detail.html:53,58,63`, `customer_detail.html:13,18,24` and
  `supplier_detail.html:20,22,24` called the deleted functions as
  `hx-on::after-request` handlers. Deleting the functions verbatim turned all nine
  into `ReferenceError` **and** dropped close-on-deactivate, which is pinned by
  `e2e/tests/test_products.py:903-904`. Binding the server event was not
  available: a deactivate fires the *same* `product-changed` as an ordinary
  in-panel save, so it would dismiss the panel on every edit. `data-drawer-close`
  is a click contract, so on a form it would collapse the panel on submit and
  take the operator's detail with them when the request then failed. The landed
  answer is a client-side `drawer:close` event dispatched on the enclosing panel
  under `event.detail.successful`, which `data-drawer-close-on` already binds —
  zero per-module functions and zero new controller code. `hx-on::after-request`
  was already the dominant idiom here (25 files), so this is not a new
  dependency.

  **2. Eight `<dialog>` modals were trapped inside the split frame.** The
  migration wrapped each page's content, and the modals sat between the list
  column and the panel, so the frame ended up with four children. A closed
  `<dialog>` is `display: none` and never becomes a grid item, which is exactly
  why all 62 e2e specs passed with the defect in place. It is still a real
  coupling: a non-modal `dialog.open = true` **is** displayed, would take
  column 1 and push the panel onto a second row. They are now siblings of
  `.drawer-split` again, direct children of `main`.

  **3. Two of the plan's own claims were wrong, and the writer corrected both
  with measurements rather than assertions.**
  - `documents.html` has **no dialog at all**. The plan listed "dialog L54" from
    a grep that had matched a backticked `` `<dialog>` `` inside the Jinja
    comment explaining that the page owns no dialog. Acting on it would have
    meant editing a phantom.
  - The plan predicted that un-trapping the modals would shrink the baseline
    diff. **It does not**: the difference set is **2418 both before and after**
    the fix, 646 of them on dialog paths. The baseline keys elements by a
    *global* sibling index (`test_visual_baseline.py:314-326`), so adding the
    wrapper renumbers `main`'s children no matter where the dialogs sit. The
    churn is caused by the split wrapper existing, not by the dialogs' position.
    The genuine signal is **10 style changes on 5 elements** — the wrapper
    computing `display` and `gap` — which is the real consequence of T3.

- [x] **T4 — raise the cap and fix the one unwrapped table.** `base.html:39`
  `max-w-[1080px]` → `max-w-[1536px]`. Add `overflow-x-auto` to
  `document_detail.html:32`, the only `<table>` in the repo without a wrapper —
  it renders inside the panel and 448 → 360/560 would squeeze it. **Delivered
  `9adad56`.** `cargo test` 1223 passed; the four drawer e2e specs 62 passed
  2 skipped; the stylesheet rebuilds with zero warnings; `document_detail.html`
  parses with zero unclosed and zero mismatched tags by a depth-tracking parser.

  **The plan named the wrong utility, and measuring caught it.** The plan said
  `max-w-7xl` "equals the `2xl` breakpoint" at 1536px. Measured in Chromium at
  1920px against the built stylesheet: **`max-w-7xl` is 1280px**, and
  `max-w-8xl` — the container that would be 1536px — is **not a utility this
  stylesheet generates at all**. Shipping the plan as written would have put the
  1920px list at **632px instead of 888px** and silently undone the entire point
  of the task. The cap is an arbitrary literal now, and the reason is recorded in
  `base.html` where a reader finds it *before* changing it back to 1080px.

  Measured geometry against the built stylesheet at the real class strings:

  | viewport | mode | main | list | panel | dead gutter |
  | --- | --- | --- | --- | --- | --- |
  | 1280 | overlay | 1024 | 960 | 538 | 0 |
  | 1440 | rail | 1184 | **648** | 448 | 0 |
  | 1600 | rail | 1344 | 808 | 448 | 0 |
  | 1920 | rail | 1536 | **888** | **560** | 64 |
  | 2560 | rail | 1536 | 888 | 560 | 384 |

  The list and the panel are **both** wider than the 448px overlay they replace,
  and 1440 holds exactly the 648px floor the threshold arithmetic was chosen for.

  **The cap is provably invisible to the visual baseline, and that was checked
  rather than assumed:** the post-change report contains **zero** width-related
  differences and is still truncated at 40 with the same first entries
  (`main[1]/dialog[2]`, T3's DOM-path shift). A class VALUE cannot change the DOM
  structure and `max-width` is excluded from `STYLE_PROPERTIES`, so T4 added
  nothing to the 2418 differences T3 caused.

  ⚠️ **My first geometry probe measured the wrong thing.** It rendered `main`
  without the cap class and so reported `main 1664 / list 1016` at 1920 —
  better-looking numbers that were not the product. The corrected probe uses the
  real `base.html` class string. A geometry probe that omits the property under
  test will confidently report the wrong answer.
- [x] **T5 — rail empty state.** New `MessageKey` (EN + ES) and the template
  fragment for the always-visible panel. **Delivered `65e8fc8`.** `cargo test`
  1223 passed including the catalog-parity and non-count-form guards; the
  stylesheet presence guard 10 passed; the four drawer e2e specs 62 passed
  2 skipped.

  **One shared key, not five.** The sentence is module-NEUTRAL because the same
  key serves products, purchases, customers, documents and suppliers. EN
  `"Select a row to see its details."` / ES `"Seleccione una fila para ver sus
  detalles."` (neutral usted register, matching the catalog). Five per-module
  keys would be ten catalog rows for a transient state. Wire name is
  **`drawer.empty`**, not a dot-less name, because **698 of 698** existing keys
  are `namespace.name` — one exception in 699 is an inconsistency someone
  "fixes" later.

  The markup is a partial (`partials/drawer_empty.html`, five identical call
  sites — the real reason a partial exists) and it **reuses the existing `.empty`
  component**, so the stylesheet needed **no rebuild**: `.empty` was already
  compiled in. Customers is the one server-rendered body, so its include sits in
  the `{% else %}` — `/customers` shows the empty state, `/customers/{id}` shows
  the statement.

  **The controller shadowing bug this surfaced.** `closeDrawer` had
  `var el = drawerNode(id)`, and the same IIFE owns an `el(tag, attrs, text)`
  DOM builder. `el('div', …)` inside that function would have called a DOM node
  and thrown. The local is now `panel`, and the comment names the trap.

  **Seven e2e assertions pinned the old empty body, and they were RED before
  this work** — the failure was literally a diff of the new
  `<div class="empty">` against an expected `''`. They encoded the defect T5
  fixes, not a behaviour worth keeping. Each was rewritten to a named helper
  asserting **exactly one `> .empty` child**, which is *stronger* than the
  `to_have_text("")` it replaces: `not_to_be_visible()` cannot tell an emptied
  body from a full one, and a leftover detail fragment carrying the same words
  would pass on the text alone.

  Verified in a real browser at 1920: the rail renders 560px wide with **no
  `data-open`** and shows the sentence; clicking a row removes the box and shows
  detail; the close button brings the box back with the panel still visible.
  `/customers` shows it, `/customers/{id}` does not.

- [x] **T6 — tests.** The two Rust assertions on the function *source strings*
  (`smoke_tests.rs:3937-3940`, `purchases_web.rs:5824-5827`) break on rename.
  Add: overlay behaviour pinned at 1280 (the default, already there) AND an
  explicit narrow viewport so the overlay path stops being effectively untested;
  split behaviour pinned at 1440; and the ✕-clears-to-empty state at 1440.
  **Delivered `00d2b21`.** The Rust half was already done in T3. The new work is
  `e2e/tests/test_drawer_layout.py` — **13 tests, no production change.**

  It lives outside every module's spec because it tests the SHARED component, and
  it covers **all five modules** because covering one would not prove the reuse
  this feature claims. The default viewport is deliberately left at 1280: moving
  it above the threshold would silently invert many existing drawer assertions.

  | test | viewport | claim |
  | --- | --- | --- |
  | the split threshold is a boundary | 1439 → 1440 | overlay at 1439, rail at 1440, **failing in both directions** |
  | the rail is there with no click ×5 | 1440×900 | visible, not `fixed`, one `.empty` child |
  | the overlay opens on a row and Escape closes it ×5 | 1280×720 | hidden, then the record's own text with **zero** `.empty`, then closed + empty state |
  | the overlay is full bleed | 360×740 | rendered width == available width, and `available < 400` so it cannot pass by coincidence |
  | the rail survives its own ✕ | 1440×900 | body empties **and** the panel stays visible — either half alone is vacuous |

  The threshold test holds one page at 1439 and then changes only the viewport,
  so the single variable is the width. Raising the threshold to `999rem` breaks
  the 1440 half; lowering it to `70rem` breaks the 1439 half. That pins the
  value **from both sides** instead of trusting the comment.

  ### The finding: one mutant killed nothing

  Every test was proven non-vacuous. The split mutation (`90rem` → `999rem`, and
  separately deleting the in-flow `.drawer` rule) fails 7 of 13. Removing the
  `min(100%, …)` bound fails exactly 1 — the full-bleed test, reporting
  `measured 400px against 360px available`. Stopping `closeDrawer` from
  re-inserting the empty state fails 6.

  **Dropping `closeDrawer`'s `if (!drawerIsRail.matches)` guard leaves all 13
  green.** The rail's permanence is the **stylesheet's** guarantee: inside
  `@media (width >= 90rem)`, `.drawer { display: flex }` is unconditional, at the
  same specificity as the base `display: none` and later in source order, so a
  rail is a column whether or not `data-open` is set. No DOM state can observe
  the controller's branch. Not a behaviour defect — the outcome is correct and
  doubly guaranteed — but `base.html`'s comment credited the controller, so it
  now states what actually provides the guarantee and warns against deleting the
  branch without re-running the mutation.

  **A test that survives the mutation it was written for is decorative coverage.**
  The writer also caught two of its own docstrings claiming discriminations the
  mutations disproved, and corrected them rather than shipping them.

  Two more things the work pinned down:
  - **Purchases needed two different markers, not one.** The list row shows the
    *supplier*; the body shows the line's *product*, because they are separate
    swaps by separate endpoints. A single string would have proved neither.
  - `--tb=line` is unreliable in this suite — it reported line 456 when the real
    failing assertion was 441. Every mutation was re-verified with `--tb=short`.

  Verified: the new spec 13 passed; the full suite **1 failed, 127 passed,
  4 skipped**, whose sole failure is T7's known `test_visual_baseline[chromium]`;
  `cargo test` 1223 passed; no stylesheet or baseline file in the diff.
- [x] **T7 — visual baseline. Delivered `e4ae700`. NOW GREEN.**
  Regenerated deliberately and audited by comparing the two JSONs directly,
  **not** by reading the test's own report — that report stops after 40
  problems and would have shown a fraction of the real diff.

  The verdict:

  - **Only `display` and `gap` ever changed** on any element that kept its DOM
    path, across all 86 captures. Not one colour, font-size, border, padding or
    radius moved. No pre-existing element was silently restyled, which is the
    rule this baseline exists to enforce.
  - Those two are the `.drawer-split` frame computing `display: grid` and
    `gap: 24px` at 1440, at five paths where a **different** element used to
    sit. The recorded old value is `flex`, not `block` — that is the tell: a
    path collision, not a restyle.
  - The remaining ~1170 differences are path renumbering. Paired by
    fingerprint, every moved element carries **identical** computed styles,
    which is what proves the tree moved rather than the appearance changing.
  - The nine `:hover-skipped` captures changed for two reasons: the dialog paths
    renumbered, and the panel's header anchor and ✕ **stopped being skipped** —
    because at 1440 the panel is a visible grid column, so the hover walk now
    visits it. That is the split becoming observable.
  - **The rail's empty state is now fingerprinted.** Fifteen captures contain a
    dashed `.empty`, up from twelve: `customers`, `documents` and `suppliers`
    went 0 → 1, and the product and purchase captures 1 → 2.
    `border-top-style` is in `STYLE_PROPERTIES`, so it is a real assertion
    rather than a hole in the net.
  - Capture count 86 before and after, as a layout-only change requires. Key
    count 5453 → 5486.

  ⚠️ **My first classification method was wrong and I corrected it.** I paired
  removals with additions on identical fingerprints and reported "1173 shifted,
  9 gone, 55 new". That pairing is unreliable here — dozens of sibling `div`s
  share identical computed styles, so it mispairs and invents phantom
  additions. The reliable signals are the style-property scan and reading the
  path prefixes directly, which show every "new" path is the same element one
  level deeper.


## Review record

`gentle-ai review assess` per work-unit commit, with the untracked inventory
declared (`--untracked-scope=exclude`) because three `odd/tasks/` documents sit
untracked and the assessor refuses to guess at them. Without that flag it
returns `unassessable`, which the contract treats as **high** — so an
unassessable result must never be read as low risk.

| commit | risk | reason | due | outcome |
| --- | --- | --- | --- | --- |
| `38f1860` T1 | medium | `executable_change` on `assets/tailwind.css` | no | `under_budget` — pending in this slice |
| `50cb4bf` T2 | medium | `executable_change` on `templates/base.html` | no | `under_budget` — pending in this slice |

Both are `under_budget`, **not** `passive`. The tier is not lowered, and a later
commit that reaches the ~400-line slice budget makes the accumulated range due.

## Constraints

- **No JavaScript width detection.** Media queries and `clamp()` only. A
  controller that sniffs the viewport is the defect, not the fix.
- **`@layer components` is the home**, following the `ui-component-tokens`
  precedent (#99): `.card`, `.notice`, `.btn-*`, `.field`. A raw utility soup in
  a template is what this feature removes.
- **One definition per fact.** The panel width is declared once, on
  `.drawer-split`. A module may not restate it.
- **`round_to_cents` and `calculate_line_taxes` are untouched.** This feature
  moves no money and computes no price.
- **`display`, `gap` and `padding-*` ARE fingerprinted** by the baseline, so
  expect real diffs on the wrapper; `max-width` and `width` are not.
- **The Tailwind v4 standalone CLI is a prerequisite.** `scripts/build-css.sh`
  resolves it from PATH or `TAILWINDCSS=`. The built file is committed so
  `cargo run` works without it — so without the CLI every new class silently
  does nothing. That is the most likely way this work appears to fail.
- **Inline `<script>` bodies are outside the stylesheet guard** (recorded at
  `src/stylesheet_tests.rs:66-83`; 27 inline scripts in this tree). The new
  controller is an inline script, so it will not be guard-covered. Safe direction
  — the guard can under-report, never false-positive — but the coverage claim is
  narrower than it looks.
- Verification: `cargo test` plus `bash scripts/e2e.sh`. The drawer is
  user-visible, so e2e is not optional.
- Never stage `odd/tasks/pos-counter-sales.md` or
  `odd/tasks/residual-interface-scope.md`. Never open `roya.db`; test pools are
  `sqlite::memory:`.
- Technical artifacts in English.

## Resolved TDD

Enabled for everything with a testable unit. Runner: `cargo test` for Rust,
`bash scripts/e2e.sh` for the browser. CSS/template work is verified by the
stylesheet guard, the Rust route assertions and the e2e specs, and proven by
deleting the rule and watching the owning test fail — the discipline the
`tailwind-stylesheet-rebuild` work established. No behaviour change ships
without a RED observation first.

## Delivery

Single branch, no PR split. Forecast ≈ 300 authored changed lines
(additions + deletions, generated files excluded) across CSS, one controller,
five templates, localisation and tests — under the ~400 heuristic. T1 is
independently reviewable and lands as its own work-unit commit.

## Branch

`feat/drawer-split-component`, branched from `696f66c` on
`feat/final-price-markup`, so it carries that branch's 7 commits. The two
features touch the same drawer templates, so branching from `main` would
guarantee a conflict; how they are ultimately split into PRs is the user's call.
