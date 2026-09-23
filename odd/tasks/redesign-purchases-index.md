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
    zero-unsaved-state design in S5 makes that structural rather than incidental.
13. **The peek is the existing fragment, not a second one.** `/purchases` opens
    `GET /web/documents/detail/purchase/{id}` into the same drawer shell the
    sibling drawers use. `/documents` and `/purchases` therefore show one peek,
    defined once. (Decided 2026-09-22.)
14. **The peek keeps its draft actions.** `Eliminar borrador` (gated
    `purchases.create`) and `Descartar` (gated `purchases.cancel`), both with a
    mandatory impact preview, stay. Read-only holds for the document's content;
    the lifecycle actions are the only writes and they are the ones an operator
    wants without leaving the list. (Decided 2026-09-22.)
15. **SKU leaves the drawer line tables.** The drawer is a view; SKU is the least
    read column and it made the table cramped with no scroll wrapper. Both
    families lose it, so `/documents` changes too. Done in S3 part A. (Decided
    2026-09-22.)
16. **The drawer keeps `max-w-md`.** A 768 px widening was considered and
    rejected: dropping the SKU column buys the space instead. (Decided
    2026-09-22.)

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
- **The 400-line budget is not holding for this feature.** S5a shipped 614 and
  S5b 608 changed lines. Both are single indivisible work units — the template
  change and the Rust flag rename cannot be separated without a broken
  intermediate, and the merge rule plus its notice plus its tests are one
  behaviour — so they are reported rather than compressed, per the skill's rule
  that the budget is not code-golf. Recorded here so the pattern is visible
  rather than discovered at PR time.
- **A "do not run the whole suite" instruction must come with the right filter.**
  S4's first round ran only `purchases_new`, `create_purchase` and the guard, and
  missed a red test in the same file that pinned the markup the slice deleted —
  so a green focused run hid a red suite until independent verification. For any
  slice that DELETES markup or renames a route, the focused filter must be chosen
  to include the tests that assert that markup, or the worker runs the full suite.

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
- [x] **S3 — The read-only peek on `/purchases`.** The list row opens the
      existing document fragment (`GET /web/documents/detail/purchase/{id}`)
      into a drawer shell; the row is one anchor carrying both the `hx-get` and a
      real `href` to the record page; the `Open` button is gone. SKU left both
      drawer line tables. Done in `ee90d3e` (part A) and `4a8ec2e` (part B).
- [x] **S4 — `/purchases/new` as a creation page.** Supplier and purchase
      date (required), supplier invoice no and notes (optional), one plain form
      posting the existing `POST /web/purchases` and landing on
      `/purchases/{id}`. `page_action_href` became `/purchases/new`, so
      `page_header.html` was not touched. The card, the two-column grid and the
      roster on the list went with it, and the action is now offered only to a
      principal holding `purchases.create`. Done in `7ae7c62`.
- [ ] **S4b — The supplier select in Edit header.** The record page's Edit
      header dialog gains the supplier field
      (`UpdatePurchaseDraft.supplier_id` already supports it; only
      `UpdatePurchaseHeaderForm` lacks it). The record page struct must then
      carry the supplier roster.
- [x] **S5a — The entry row; the drawer dies.** One persistent flex row inside
      the swapped money region (product, qty, cost, Add), the results below it so
      they cannot move the button, and every pick commits the line with the typed
      quantity — so no line is ever unsaved. Deleted: the drawer, the keep-open
      checkbox and its `localStorage` preference, the open/close functions, the
      afterRequest close, the action-bar Add line button and its anchor
      interceptor, and the out-of-band picker copy. A draft record page no longer
      offers a header action. Done in `34b19ff`.
- [x] **S5b — A1: a repeat product increments instead of 400.** The domain rule
      (one line per product) stays. A repeat add merges into the existing line
      only when the resolved cost equals that line's cost; a different cost keeps
      the 400, because that is exactly the case the rule exists to catch — one
      product cannot carry two prices on one purchase. The rejection reuses the
      same function the strict path uses, so the message cannot drift. The merge
      renders a server-rendered notice naming the product and the new quantity.
      The web route merges; the JSON API keeps the strict 400 deliberately. Done
      in `9eb8187`.
- [ ] **S6 — List row + status palette.** Responsive grid row (identifier,
      supplier, date/items, total, state); the total stops encoding payment
      state; a status chip carries paid / pending / overdue; the new warning
      token; the technical subtitle deleted from `/purchases` and `/sales`.
- [ ] **S7 — Sales mirror.** The same row and peek on `/sales`.
- [ ] **S8 — One language, one date format.** Centralized labels +
      `format_date`; English across the sidebar, the document groups, the peek's
      operator copy (`Editar cabecera`, `Abrir el documento`, `Proveedor`,
      `Líneas`, …) and the audit labels still Spanish in ten partials
      (`Registrado por`, `Actualizado por`, `Sugerido`); specs and the tests that
      assert the Spanish labels updated.
- [ ] **S9 (optional) — Unified `q` search.** `PurchaseListQuery` collapses
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
| S3 | **done** | `ee90d3e` (part A) + `4a8ec2e` (part B) — 5 files. Worker observed RED for both parts (drawer headers `left: ["Producto", "SKU", …]`; `/purchases` missing the peek shell) then GREEN. `gentle-ai-verify` verdict *pass-with-caveats* on `cargo test` 842 passed / 0 failed: header/cell counts read as 4 and 4 in both tables, exactly 4 deleted lines, no nested anchor, Escape handlers statically disjoint, no new route or Tailwind utility, guard green. Both caveats fixed before commit: an orphan wrapper `div` left by the deleted `Open` anchor, and a smoke comment/assertion that still claimed the drawer rendered SKU (the token matched the product's name, so it proved nothing). Parent review also caught the writer titling the new shell `Compra` — new Spanish on an English page — and changed it to `Purchase`. |
| S4 | **done** | `7ae7c62` — 5 files. Worker RED for three contract tests, then GREEN. First verification pass returned **FAIL**: `cargo test` was red on `web_create_draft_form_asks_only_supplier_and_purchase_date`, a pre-existing test that pinned the deleted card and forbade the invoice field the page must now have. The fix round rewrote it to the new contract (keeping the prohibition that still matters: no payment decision in the creation form), pinned `/purchases/new`'s 403, and extracted the guard's non-vacuity decision into `wiring_is_vacuous` with a three-boundary mutation pin. Second pass verdict *pass* on `cargo test` 848 passed / 0 failed, run twice by the verifier. |
| S4b | not started | — |
| S5a | **done** | `34b19ff` — 5 files, 614 changed lines (over the 400 budget; one indivisible work unit, reported not compressed). Verifier verdict **pass** on `cargo test` 849 passed / 0 failed **and** the browser suite: `uv run pytest tests/test_picker.py` → 6 passed in Chromium against the working-tree binary. The verifier confirmed the browser-silent contracts by source (the ids the results fragment's `hx-include` and htmx's id-based re-focus depend on, and the `hx-disinherit` rule), that the shared macro and the sale record are untouched, and that the preview test's narrowed slice is a strict subset that hides no payment input. It named one coverage gap — the record page-action slot was pinned in neither direction — closed before the commit. |
| S5b | **done** | `9eb8187` — 5 files, 608 changed lines (over budget; one indivisible work unit). Verifier verdict *pass* on `cargo test` 854 passed / 0 failed and the browser suite 7 passed, having checked the equality branch (`Decimal` numeric equality, no tolerance), that the different-cost arm returns through the same function so the message cannot drift, that the `unreachable!` is genuinely unreachable, that the out-of-band notice cannot leak into the `hx-select` main swap (traced in the vendored htmx 1.9.12), and that `add_line` is byte-identical apart from the extracted helper. It raised three findings, all fixed before the commit: a new dead-code warning (`changed_with_entry_row` orphaned by `changed_with_notice`), the merge notice's escaping untested, and an undocumented ordering divergence — `add_or_increment_line` resolves the cost before the uniqueness check because the merge needs it, so a repeat with an invalid explicit cost reports the cost error rather than the duplicate one. |
| S6 | not started | — |
| S7 | not started | — |
| S8 | not started | — |
| S9 | not started (optional) | — |

## Open decisions

- **`redesign-interface` bookkeeping.** Its R3/AC5 survive intact under this
  design (the record page is untouched and the row keeps a real `href`), so the
  change needs a note that `/purchases` now peeks instead of navigating, not a
  page reversal.
- **Quick-create supplier.** Not built: `new-supplier-dialog` exists only in
  `suppliers.html`, so a missing supplier still forces a detour to `/suppliers`.
  Cheap to add later (`POST /web/suppliers` exists), and it must not nest a
  `<dialog>` inside the creation page's form.
