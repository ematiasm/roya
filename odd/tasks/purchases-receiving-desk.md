# Feature: Purchases "Receiving desk" (Proposal A)

## Objective

Redesign the purchase detail and add-line UX into a "receiving desk": one primary
action per status via a sticky action bar, an add-line right drawer, a confirm
`<dialog>` that is the ONLY place a payment method appears, an effects preview,
and inline line editing — all in English, reusing the project's existing
drawer/dialog/HTMX patterns.

## Problem

- Draft detail shows 4 equal action cards (Add line / Confirm / Edit header /
  Discard): no hierarchy, confirm has same weight as discard.
- Payment-method UI sits inside the draft confirm card even though payment only
  posts when the purchase is confirmed.
- Add-line is an inline card far from the scanner workflow; no batch receiving.
- Lines are delete-only (REST PUT exists, no web route).
- Mixed language: "Sugerido", "Registrado por", "Actualizado por" in an
  otherwise English UI.

## Why

User requested a UI/UX analysis and two purchase interface proposals; proposal A
was deepened and fully approved for implementation (2026-09-22).

## Authorized scope

- Templates: `templates/purchase.html`, `templates/purchases.html`,
  `templates/partials/purchase_detail.html`, new partials under
  `templates/partials/`, English renames in purchase-related partials
  (`purchase_list.html`, `suggestion_list.html`, page strings rendered by
  purchases routes).
- Rust: `src/routes/purchases_web.rs` (context strings, NEW web route for line
  update), `src/models.rs` / service queries as needed to add `track_stock` to
  purchase lines, `src/smoke_tests.rs` (update assertions).
- E2E: `e2e/tests/test_purchases.py`, `test_picker.py`, `test_confirmation.py`
  updates when DOM contracts change.
- NO backend behavior changes beyond: (a) `track_stock` enrichment read-only,
  (b) new web route that wraps the existing line-update service/repo capability.

## Locked design decisions

1. UI language for touched surfaces: **English**.
2. **Payment method appears ONLY in the Confirm `<dialog>`** (and only for Cash).
   Draft shows an "Effects preview" with projections only — no payment inputs.
3. **Add line = right drawer**, exact project pattern
   (`fixed inset-y-0 right-0 z-40 w-full max-w-md`, no backdrop, HTMX body
   swap; source pattern: `templates/documents.html`).
4. Effects preview **enriches lines with `track_stock`** → precise
   "+N units (tracked)" messaging.
5. Drawer has **"Keep open after adding"** checkbox (default OFF, persisted in
   `localStorage`).
6. Confirm step: click `Confirm ▾` (action bar) opens `<dialog>` (existing
   `showModal()` pattern). Cash → method select + derived account; Credit → no
   select, shows due date. Submit posts to existing
   `POST /web/purchases/:id/confirm`.
7. Keep element ids `#add-line`, `#confirm-purchase`, `#edit-header`,
   `#discard-purchase` on their new homes (bar/menu/dialog) to minimize
   `smoke_tests.rs` blast radius.
8. Sticky action bar primary per status:
   Draft → `Confirm ▾` (disabled with 0 lines);
   Confirmed+Credit → `+ Record payment`;
   Confirmed+Cash → none; Cancelled → read-only.
   Secondary via `⋯` menu: Edit header / Discard draft (Draft),
   Cancel purchase (Confirmed).
9. Inline line edit: qty/cost inputs, `change delay:400ms`, NEW web route
   wrapping existing update capability; invalid → notice + revert.

## Constraints

- Reuse existing patterns only (drawer, `<dialog>`, `line_picker`, notice,
  `hx-disinherit` rules documented in `product_search_results.html`).
- Server stays authoritative; preview/dialog never replace validation.
- RDD is OFF: no review ceremony; ordinary checks only.
- No push/PR unless user asks. Delivery: work-unit commits on this branch.

## Delivery strategy

- `ask-on-risk`, forecast > 400 lines → chain strategy chosen by user:
  **`stacked-to-main`** (collected 2026-09-22). Split into stacked PRs at
  delivery time; record slice boundaries here when pushing.
- Skill: `work-unit-commits`
  (`/home/mamull/.config/opencode/skills/work-unit-commits/SKILL.md`).

## TDD / checks

- **Strict TDD: ON** — source: `openspec/config.yaml` (`strict_tdd: true`).
- Test runner: `cargo test` (primary). E2E (Playwright/pytest, secondary):
  `e2e/tests/test_purchases.py`, `test_picker.py`, `test_confirmation.py`
  when DOM contracts change.
- RED first: failing assertion (cargo smoke test or e2e) before the change it
  pins; then GREEN; then REFACTOR.

## Checklist

- [x] T1 English unification of purchase surfaces (strings + update pinned
      smoke assertions RED→GREEN).
- [x] T2 `track_stock` enrichment on purchase lines (models/query + tests).
- [x] T3 Sticky action bar + `⋯` secondary menu replacing the 4-card Draft
      grid; status-based primary (keep legacy ids).
- [x] T4 Effects preview partial (draft, projections only: stock/cash/due;
      no payment inputs; Confirm disabled at 0 lines).
- [x] T5 Confirm `<dialog>` (Cash: method select + account; Credit: due
      summary, no select) posting to existing confirm route.
- [ ] T6 Add-line drawer (documents.html pattern + `line_picker` + keep-open
      checkbox in localStorage; OOB close/clear; focus return).
- [ ] T7 Inline line edit (inputs + NEW web route + revert on invalid).
- [ ] T8 Full verification: `cargo test` green; e2e purchase suites green;
      update this doc with evidence per task (commit hashes).

## Route declaration

- T1–T7: delegated direct (writer trigger: 2+ non-trivial files; mapping
  already done inline during proposal).

## Progress

- [x] T1 done. RED: `cargo test audit_the_purchase_record_shows_the_actor_display_name`
      + `cargo test web_purchases_page` → 3 failed (English assertions vs Spanish
      templates). GREEN after templates: `Registered by`/`Updated by` in
      `purchase_detail.html`, `Suggestions`/`Suggested` in `purchases.html`,
      `suggested` in `suggestion_list.html`, English comments in
      `purchases_web.rs`; re-run audit 1 passed, web_purchases_page 2 passed,
      `cargo test ac21_the_suggestions_block` 1 passed. Remaining
      `Registrado por` occurrences are non-purchase surfaces (out of scope).
      Commit hash recorded at T8.
- [x] T2 done. RED: new service test `record_tracked_units_sums_only_stock_tracking_lines`
      failed to compile (`E0609: no field tracked_units`). GREEN: added
      `PurchaseRecord.tracked_units` (models.rs, doc-commented as the
      projection base) computed inside `record_from_detail` from the same
      per-line `tracks_stock` flags (`tracked_units += line.qty` when
      tracking); pins tracked=3 / service line excluded + both per-line
      flags. Re-run: test 1 passed; `cargo test purchase` 119 passed,
      `cargo test document` 58 passed. Commit hash recorded at T8.
- [x] T3 done. RED: new `web_purchase_record_renders_sticky_action_bar_and_status_dialogs`
      failed (no `purchase-action-bar`). GREEN: the 4-card grid became a sticky
      bar (`sticky top-14 md:top-0 z-20`, after the identity header, outside
      `record_money`) with status primaries (Draft `Confirm ▾` disabled at 0
      lines + `#add-line` drawer button; Confirmed-Credit `+ Record payment`;
      Confirmed-Cash menu only) and a `⋯` menu; action forms moved into
      `<dialog>`s keeping the legacy ids; add-line drawer (documents shell,
      `line_picker` untouched) hosts the picker; `oob_action_bar` rides with
      the OOB picker (verified against htmx 1.9.12 source: OOB runs before
      `hx-select`); all page-shell JS (menu/drawer/Escape/header-anchor) in
      `purchase.html`; both `assert_oob_picker_is_empty_and_focused` helpers
      re-pinned to find the OOB picker by tag, not the first OOB. Tailwind
      regenerated (`scripts/build-css.sh`). Re-run: new test 1 passed,
      `cargo test purchase` 120, `picker` 8, `line_picker` 2, `sale` 136,
      wiring guards       (`seeded_pages_render_only_wired`, `wiring_guard` 15,
      `fragment_external_selectors`) green. FULL `cargo test`: 801 passed.
      Commit hash recorded at T8.
- [x] T4 done. RED: new `web_purchase_record_effects_preview_projects_confirm_without_payment_inputs`
      failed (no `effects-preview`; two setup fixes en route: tracked products
      need min/max stock, nested async helper instead of a FnOnce closure).
      GREEN: new partial `purchase_effects_preview.html` (macro, Draft-only,
      called inside `record_money` so add-line swaps refresh it) with
      `Stock · +N units (tracked)|no stock movement` from
      `record.tracked_units.is_zero()` (Askama rejects `Decimal > 0`),
      `Cash · -$total at confirm|no cash movement`, `Due · $0|+$total at
      confirm` — projections only, zero inputs in the preview (pinned), and
      Confirm `disabled` at 0 lines / enabled with a line (triangulated).
      Re-run: new test 1, `web_purchase_record` 11, `purchase` 121, `money` 7.
      FULL `cargo test`: 802 passed. Commit hash recorded at T8.
- [x] T5 done. RED: new `web_purchase_record_confirm_dialog_carries_payment_method_only_for_cash`
      failed (select present but not `required`, empty Credit option shown).
      Setup fixes en route: `seed_record_fixture` now suffixes product SKU,
      supplier name, and account name with a process counter (accounts.name
      is UNIQUE; one test may seed several fixtures — the defaults helper
      still receives the canonical "Caja"). GREEN: the confirm dialog branches
      on payment type — Cash renders `#confirm-method` with `required` and no
      empty option (the only `name="method_id"` on a draft page), Credit
      renders a due summary (`$total due on {date} after confirm`) with zero
      method controls on the page. Server confirm behavior untouched (Credit
      with a method already 400s; Cash without one already 400s). Re-run:
      new test 1, `purchase` 122, `wiring` 15. FULL `cargo test`: 803 passed.
      Commit hash recorded at T8.
- [ ] All remaining tasks pending. Next step: T6.

## Acceptance criteria (feature-level)

1. Draft purchase page: no payment-method control anywhere; Effects preview
   visible; Confirm disabled with zero lines; action bar sticky under header.
2. Confirm flow: Cash requires method in dialog before submit; Credit dialog
   has no method control; server confirm behavior unchanged.
3. Add-line: drawer opens from `+ Add line`, scan→add loops when keep-open is
   checked, drawer/OOB behavior does not leak `hx-*` inheritance (hx-disinherit
   preserved).
4. Lines editable inline; invalid input reverts with notice; totals refresh.
5. All touched UI strings English; existing legacy ids present.
6. `cargo test` passes; affected e2e suites pass.
