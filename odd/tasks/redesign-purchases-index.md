# Feature: Redesign Purchases index (and its Sales mirror)

## Objective

Turn `/purchases` into a scannable operational list: no REST API surface in the
operator's view, one row per record with a real reading order, status color that
means status, creation behind a `<dialog>` instead of a permanent card, and one
language plus one date format across the interface. Mirror the shared parts on
`/sales`, which is the same page shape.

## Problem

Read from the current templates on 2026-09-22:

1. **The REST API card is on the operational page.** `templates/purchases.html:95`
   lists 20 endpoints in the middle of the buying workflow. The same card is on
   `sales.html:106`, `suppliers.html:27` and `dashboard.html:126` — three
   redundant copies of a surface the shell already links in its footer
   (`partials/sidebar.html` → `REST API ↗`, guarded by `smoke_tests.rs:3600`).
2. **The row has no reading order.** `partials/purchase_list.html` opens with
   three badges (status, payable/settled, payment type) before the supplier, then
   repeats the money line twice: `total • paid • due` in the muted line and the
   total again on the right.
3. **Red means two things at once.** `assets/tailwind.css` defines
   `--color-danger: #f87171` and `--color-expense: #f87171` — the same hex. The
   list paints the total with `text-expense` whenever `due.is_sign_positive()`,
   so a draft's total is red for no reason an operator can act on, and a red
   total cannot also mean "problem".
4. **The primary action is welded to a card.** `purchases.html` sets
   `page_action_href = "#new-purchase"`, a plain anchor to the New Purchase
   (Draft) card. Deleting the card without replacing its target breaks the
   primary action.
5. **A technical subtitle sits above the list.** `purchases.html:10` prints
   `purchases • negative stock allowed • overdraft blocked` — a global config
   echo on a per-page basis. Same in `sales.html:10`.
6. **Two languages and two date formats.** The chrome is English, while the
   sidebar renders `Usuarios`, `Roles`, `Cerrar sesión` and the document group
   filter renders `Ventas`, `Compras`, `Movimientos de stock`
   (`src/models.rs:1295`). Read views print ISO (`2026-09-17`) while a
   `<input type="date">` renders in the browser locale (`dd/mm/aaaa`).

## Why

The user submitted a UI/UX proposal for the purchases view on 2026-09-22 and
approved it after review. The review corrected three of its premises (the rows
are already compact rows, not cards; the red total keys off `due`, not payment
type; the primary action depends on the card it wanted deleted) and split one
item out as domain work rather than presentation.

## Authorized scope

- Templates: `templates/purchases.html`, `templates/sales.html`,
  `templates/suppliers.html`, `templates/dashboard.html`,
  `templates/partials/purchase_list.html`, `templates/partials/sale_list.html`.
- Palette: `assets/tailwind.css` + regenerated `static/tailwind.css`.
- Rust: `src/routes/purchases_web.rs`, `src/routes/sales_web.rs` (view structs
  for the list rows), `src/smoke_tests.rs` (page-level contract assertions).
- i18n/date slice only: a new labels/format module, `src/models.rs`
  (`DocumentGroup::label`), the sidebar strings, and the date rendering in read
  views.
- E2E: `e2e/tests/test_purchases.py`, `e2e/tests/test_parties.py` when a DOM
  contract changes.

## Locked design decisions

1. **Interface language: English, with strings centralized.** Decided by the
   user 2026-09-22. The English translation covers the domain vocabulary too
   (`Ventas`/`Compras`/`Movimientos de stock` → English), and the strings live in
   one server-side place so a later language map is a new table, not a grep over
   22 templates. Date format is likewise one helper applied to read views.
2. **`<input type="date">` values stay ISO.** The locale rendering in the filter
   is the browser's, not a data inconsistency; only read views get a formatter.
3. **The list is a responsive grid row, not a `<table>`.** A real data grid needs
   `overflow-x-auto` (inner scroll on a phone) or a fragile `display:block`
   rewrite to survive 360 px, and it would introduce a second list idiom. A grid
   row gives the same aligned columns with one markup and no horizontal scroll.
4. **Status color comes from a status chip, never from the total.** The total
   becomes neutral. `--color-expense` keeps its money-out meaning; a new warning
   token carries "pending".
5. **No typed identifiers.** Inherited from `redesign-interface` R2.
6. **Server stays authoritative.** Any business rule change is named as such and lands in its own slice with its own tests; nothing else here touches a rule.
7. **A1 — one line per product, and a repeat scan increments it.** The domain
   rule ("a product may appear only once per purchase, for the same reason as
   sales") stays. Adding or scanning a product already on the draft increments
   the existing line's quantity through the existing update route, with visible
   feedback, instead of the current 400. (Decided 2026-09-22.)
8. **B1 — an empty line cost means this supplier's cost.** `add_line` resolves
   the purchase's supplier satellite cost first and falls back to
   `products.cost_price` only when that supplier has no satellite row. Done in
   S2. (Decided 2026-09-22.)
9. **The record is hosted in a right panel, and the receiving desk is retired.**
   Option 2 of the panel review (2026-09-22). The nested add-line drawer, the
   "keep open after adding" checkbox, its `localStorage` preference and the OOB
   picker swap all go — those are what made the desk tosco. The status-gated
   action bar, the effects preview, the inline line edit, the `⋯` menu and the
   dialogs stay; the action bar moves to a sticky footer inside the panel.
10. **The panel is addressable, so R3/AC5 are preserved rather than reversed.**
    `/purchases/{id}` renders the list with the panel open server-side.
    Precedent: `templates/customers.html:75` (`{% if drawer_open %}`) fed by
    `src/routes/customers_web.rs:315`. Reload, back button and bookmarks keep
    working, and the record fragment route `/web/purchases/{id}` stays for
    in-page navigation.
11. **Cabinet data.** The panel header shows identifier, supplier, date and
    status as read-only facts; editing opens the existing Edit header dialog,
    which gains the supplier select — the domain already supports it
    (`UpdatePurchaseDraft.supplier_id`, `src/models.rs:1048`; `update_draft`
    validates and persists it), only the web form `UpdatePurchaseHeaderForm`
    lacks the field.
12. **Drafts are already persisted server-side.** Every mutation is an immediate
    POST; there is no client-side cart and no document state in the browser. The
    zero-unsaved-state design in S4 makes that structural rather than incidental.

## Out of scope, parked deliberately

- **API hint lines inside forms.** `templates/dashboard.html:80`,
  `templates/customers.html:67`, `templates/suppliers.html:45`,
  `templates/partials/product_detail.html:252` and
  `templates/partials/supplier_detail.html:89` still print
  `Also at <code>POST /api/…</code>` under a form's submit button. Same defect
  class as the card, but five surfaces outside this feature's pages. The
  purchases and sales hints sit inside the `#new-purchase` / `#new-sale` cards,
  so S3 removes them with the cards.
- **Filter bar decluttering** (advanced-filters panel, dropping `Refresh`, a `✕`
  inside the search field). Parked by user decision 2026-09-22: it is the only
  part of the proposal that touches a component shared by four pages
  (`partials/list_filters_close.html`), it has **no test coverage at all** today
  (no assertion exists for `Refresh` or `Clear`), and the proposed `✕` would
  trade away a `Clear` anchor that works without JavaScript. If S5 happens, it is
  absorbed there because both touch the same `<form>`.
- **Unified free-text search** — domain work, isolated in S5.
- **Sorting and multi-select** on the list — a new capability, not a redesign.
- **Counter mode (POS), ticket printing, PWA** — already parked by
  `redesign-interface`.
- Business rules, migrations, schema, API surface.

## Constraints

- Reuse existing patterns only: the `showModal()` `<dialog>` pattern
  (`suppliers.html`), the grid-row idiom (`min-[900px]:grid-cols-[…]` already in
  `purchases.html`), the notice region, the shared page header.
- Tailwind CSS must be regenerated (`scripts/build-css.sh`, standalone CLI on
  PATH) in every slice that adds classes, and committed: `cargo run` must work
  without the CLI installed.
- RDD is **OFF** (`gentle-ai review mode status` → off, global): no review
  ceremony, ordinary checks only.
- No push and no PR unless the user asks. Delivery is work-unit commits on
  `feat/redesign-purchases-index`.

## Delivery strategy

- One slice per commit group; every slice is a chained-PR candidate, all
  targeting `main`, merged strictly in order.
- Budget: 400 changed lines per slice (default). Report the overage rather than
  compressing comments or tests.
- Skill: `work-unit-commits` (tests and docs travel with the behavior they
  verify).
- **Re-slice (2026-09-22, after the panel decision).** The plan was re-cut twice.
  First the palette token folded into the row slice, because a commit that only
  adds unused CSS variables has no consumer and is not a work unit. Then the
  record-in-a-panel decision split the old S2/S2b/S3 apart and put the
  satellite-cost defect first, because the panel would have displayed and
  pre-filled that wrong number. The palette now rides in S5 with the list row.
- **Slice list**: S1 (REST API cards) done, S2 (satellite cost default) done,
  S3 (record panel + shared record script + responsive line rows), S4 (entry
  row; the add-line drawer dies), S5 (list row + status palette), S6 (sales
  mirror), S7 (one language, one date format), S8 (optional: unified `q`
  search).

## TDD / checks

- **Strict TDD: ON** — source: `openspec/config.yaml` (`strict_tdd: true`).
- Runner: `cargo test` (primary). E2E (Playwright/pytest, secondary) when a DOM
  contract changes: `uv run pytest` under `e2e/`.
- RED first: a failing page-level assertion before the change it pins, then
  GREEN, then REFACTOR.

## Checklist

- [x] **S1 — Remove the REST API cards.** Page-level contract tests for
      `/purchases`, `/sales`, `/suppliers` and `/` (RED), then the four cards
      deleted (GREEN). Model: `smoke_tests.rs:3656` +
      `e2e/tests/test_products.py:105`. The sidebar `REST API ↗` link stays.
      Done in `8e22377`.
- [x] **S2 — Empty line cost defaults to the supplier's satellite cost.**
      `add_line` resolves
      `SupplierService::find_cost(product_id, purchase.supplier_id)` first and
      falls back to `products.cost_price` only with no satellite row. Four unit
      tests pin the order; the picker label and two stale test messages were
      corrected in the same commit. Done in `737b1bd`.
- [ ] **S3 — The record panel.** `/purchases/{id}` renders the list with the panel
      open server-side; the record fragment loads into it over HTMX for in-page
      clicks; the receiving-desk script is extracted to a partial shared by
      `purchase.html` and `purchases.html`; the panel width and the line-row
      stacking below its breakpoint are decided and implemented.
- [ ] **S4 — The entry row.** Picking a product commits a line immediately at
      qty 1, so there is no unsaved state; the qty field acts as an optional
      multiplier that resets; a repeat product increments the existing line (A1).
      The add-line drawer, the keep-open checkbox, its `localStorage` preference
      and the OOB picker swap are deleted. `e2e/tests/test_picker.py` is rewritten
      to the new order.
- [ ] **S5 — List row + status palette.** Responsive grid row (identifier,
      supplier, date/items, total, state); the total stops encoding payment
      state; a status chip carries paid / pending / overdue; the new warning
      token; the technical subtitle deleted from `/purchases` and `/sales`.
- [ ] **S6 — Sales mirror.** The same row and panel on `/sales`.
- [ ] **S7 — One language, one date format.** Centralized labels +
      `format_date`; English across the sidebar, the document groups and the
      audit labels still Spanish in ten partials (`Registrado por`,
      `Actualizado por`, `Sugerido`, `Abrir`); specs and the tests that assert
      the Spanish labels updated.
- [ ] **S8 (optional) — Unified `q` search.** `PurchaseListQuery` collapses
      `supplier` + `number` into one free-text field; service + repo change.

## Acceptance criteria

- [ ] AC1: no page renders a `REST API` card; the shell's `REST API ↗` link still
      renders exactly once.
- [ ] AC2: `/purchases` and `/sales` render one row per record with aligned
      columns at ≥900 px and a stacked reading order below, and no page scrolls
      horizontally at 360 px.
- [ ] AC3: the list total is not colored by payment state; a status chip carries
      paid / pending / overdue color.
- [ ] AC4: a draft row shows a stable identifier when `purchase_number` is NULL.
- [ ] AC5: total, paid and due are printed once per row.
- [ ] AC6: the `purchases • negative stock allowed • …` subtitle is gone from
      `/purchases` and `/sales`.
- [ ] AC7: `New purchase` opens a `<dialog>`, the `#new-purchase` card no longer
      exists, and the page renders a single column.
- [ ] AC8: creating from the dialog still navigates to `/purchases/{id}`.
- [ ] AC9: every interface string is English and comes from the central labels
      module; read views print one date format from one helper; `<input
      type="date">` values remain ISO.
- [ ] AC10: `cargo test` green at every slice, and the committed stylesheet is
      up to date with the templates.

## Progress

| Slice | Status | Commits / evidence |
|-------|--------|--------------------|
| S1 | **done** | `8e22377` — 4 templates, 4 contract tests, stylesheet regenerated. Worker observed RED (4 failures: "the REST API card must not be rendered on …") then GREEN; `gentle-ai-verify` confirmed `cargo test` 834 passed / 0 failed, the wiring guard `seeded_pages_render_only_wired_htmx_targets` green, and the surviving sidebar link intact. Parent review caught and fixed two writer defects before commit: an unrelated HTML comment deleted from `dashboard.html`, and a stale test cross-reference (`sidebar_renders_*` → `sidebar_groups_navigation_into_operation_catalogue_and_cash`). |
| S2 | **done** | `737b1bd` — `fix(purchases)`, 5 files. Worker RED: `left: 5 / right: 9.50` ("empty cost must default to the supplier's satellite cost") then GREEN. `gentle-ai-verify` verdict *pass* on `cargo test` 838 passed / 0 failed, with the `Some(c)` branch byte-identical to HEAD and 2 of the 4 new tests confirmed as genuine RED pins (the other 2 are triangulation). The verifier raised two caveats, both fixed inside the commit: the picker label `Unit cost (empty = product cost)` had become false, and two test messages still stated the old unconditional rule. |
| S3 | not started | — |
| S4 | not started | — |
| S5 | not started | — |
| S6 | not started | — |
| S7 | not started | — |
| S8 | not started (optional) | — |

## Open decisions

- **Panel width.** `w-full` below a breakpoint and a cap above it is free (the
  shell already does `w-full max-w-md`). The real work is the line rows, which
  cannot hold five columns at phone width and must stack. The cap value and the
  stacking breakpoint are still to be confirmed.
- **Supplier/date in the panel header.** Read-only facts plus the Edit header
  dialog (recommended: an always-editable supplier select in a narrow panel is
  one mis-click away from moving a whole draft) versus inline editable selectors.
- **`redesign-interface` bookkeeping.** Its R3/AC5 survive intact under this
  design, so the change needs a note that the record view is now list+panel, not
  a page reversal.
