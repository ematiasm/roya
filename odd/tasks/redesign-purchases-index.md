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
6. **Server stays authoritative.** Presentation only; no business rule changes in
   S1–S4.

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
- Budget: 400 changed lines per slice (default). S2 is the only one near it; if
  it crosses, report the overage rather than compressing comments or tests.
- Skill: `work-unit-commits` (tests and docs travel with the behavior they
  verify).
- **Re-slice note (2026-09-22)**: the palette token slice was folded into S2. A
  commit that only adds unused CSS variables has no consumer, no behavior and
  nothing a test can assert, so it is not a work unit. The token and its use
  ship together.

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
- [ ] **S2 — Purchase list as a responsive grid row + semantic status palette.**
      New warning token; status chip carries color; total neutral; money line
      printed once; draft fallback when `purchase_number` is NULL; technical
      subtitle deleted from `/purchases` and `/sales`; sales list mirrored.
- [ ] **S3 — `New purchase` as a `<dialog>`.** Dialog carries the supplier roster
      the page already renders; `#new-purchase` card deleted; the two-column grid
      collapses to one column; sales mirrored. Creating still lands on
      `/purchases/{id}`.
- [ ] **S4 — One language, one date format.** Centralized labels + `format_date`;
      English across the sidebar and document groups; specs and the tests that
      assert the Spanish labels updated.
- [ ] **S5 (optional) — Unified `q` search.** `PurchaseListQuery` collapses
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
| S2 | not started | — |
| S3 | not started | — |
| S4 | not started | — |
| S5 | not started (optional) | — |
