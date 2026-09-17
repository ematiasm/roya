# Tasks: redesign-interface

## Review Workload Forecast
- Estimated: ~1700-1900 lines across four slices, almost all templates and routing. No migrations.
- The shell slice touches every template, so its diff is wide even though each change is small.
- Chained PRs recommended: Yes — 4 slices, one PR each or one PR per two slices if a slice lands small.
- 400-line budget risk: High for a single PR.
- Decision needed before apply: Yes.

## Slice N1 — shell, navigation and feedback
- [ ] T1: `partials/sidebar.html` with the three groups, plus the shell layout in the base template and the
      responsive behaviour (full, icons-only, drawer)
- [ ] T2: page header component: title, optional breadcrumb, one primary-action slot
- [ ] T3: active nav key threaded through every page's template struct and asserted in a test
- [ ] T4: replace the blocking error dialog with the notice region, and confirm no template or script calls
      the alert function any more
- [ ] T5: the drawer toggle and Escape handler as the only new script, kept under a screenful
- [ ] T6: fix the stale document title block
- [ ] T7: update every page to the shell, add each to the wiring guard, regenerate the stylesheet

## Slice N2 — record pages
- [ ] T8: `/sales/{id}` and `/purchases/{id}` handlers and full page templates, reusing the detail data
- [ ] T9: the list pages link to records and drop the side-panel detail
- [ ] T10: creating a sale or purchase answers a redirect to the new record
- [ ] T11: actions gated by status in the template, with the service still the authority
- [ ] T12: confirmation on destructive actions
- [ ] T13: quick-create a customer from the sale page without leaving it
- [ ] T14: tests for AC4, AC5, AC6, AC7

## Slice N3 — line entry
- [ ] T15: product search endpoint matching name, SKU and barcode, read-only and bounded
- [ ] T16: picker field with debounce and a results fragment
- [ ] T17: submit resolution order — barcode, then SKU, then id — so an exact barcode is one step
- [ ] T18: the response returns the updated line table, an out-of-band empty focused picker and the running
      total, so the loop needs no cart script
- [ ] T19: tests for AC8 to AC12, including a whole sale loaded through the picker without touching the
      mouse
- [ ] T20: regenerate the stylesheet

## Slice N4 — names and search
- [ ] T21: resolve product, account, method, customer and supplier names in every fragment and record page
- [ ] T22: list filters for sales and purchases: status, party, number, date
- [ ] T23: product filters for name, SKU and barcode, keeping the category filter
- [ ] T24: tests for AC13 and AC14, and a guard assertion that no rendered page shows a bare entity id
- [ ] T25: remove the typed-id rule from the wiring guard and confirm the guard still catches the mutations
      it used to
- [ ] T26: README updated for the new navigation and the record pages

## Verify
- [ ] `cargo test` green at the end of every slice, the wiring guard passing over every page, the
      stylesheet regenerated, and a manual pass at 1024 px, 768 px and 360 px widths
- [ ] Confirm no `alert()` remains and that a deliberately failed action renders a dismissible notice

## Archiving
- [ ] On merge: fold the interface requirements into the canonical specs as a presentation section, note
      the record-page routes in the `sales` and `purchases` capabilities, and move this change to
      `openspec/changes/archive/`.
