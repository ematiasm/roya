# Tasks: redesign-interface

## Slice order, and why
The user's priorities decide the order: loading a sale, loading a purchase, then search and filters.
Converting the remaining templates to the page-header pattern is **cosmetic** and moves to the end, so it
never delays a flow fix. Customer creation and debt collection were explicitly not a pain point and stay
behind the others.

## Review Workload Forecast
- Estimated: ~1700-1900 lines in total, almost all templates and routing. No migrations.
- Record-page slices touch services lightly, only to resolve display names for what the record shows.
- Chained PRs recommended: Yes, one slice at a time.
- 400-line budget risk: High for any single PR that spans sales and purchases together.
- Decision needed before apply: Yes.

## Slice N1a — shell, navigation and feedback (DONE)
- [x] T1: sidebar with the three groups, shell layout, responsive full / icons-only / drawer
- [x] T2: page header component: title, optional breadcrumb, one primary-action slot
- [x] T3: active nav key threaded through every page struct and asserted in a test
- [x] T4: notice region replacing the blocking dialog, with the failed action named
- [x] T5: drawer toggle and Escape as the only new script
- [x] T6: stale document title fixed
- [x] T7: dashboard and products converted as the two reference shapes

## Slice N2 — sale record page
- [ ] T8: `/sales/{id}` as a real page inside the shell, with the header, totals and payment status
- [ ] T9: lines as a table with product name and SKU, quantity, unit price and subtotal
- [ ] T10: payments with the account name and the method name, not their ids
- [ ] T11: actions in context, gated by status, with confirmation on cancel
- [ ] T12: creating a sale redirects to its record; the list links to records and loses the side panel
- [ ] T13: remove the typed-id forms from the sales page
- [ ] T14: tests for AC4, AC5, AC6, AC7 and the name resolution
- [ ] T15: regenerate the stylesheet

## Slice N3 — purchase record page
- [ ] T16: `/purchases/{id}` mirroring the sale record, including supplier name and cost per line
- [ ] T17: actions gated by status, confirmation on cancel, list links to records
- [ ] T18: remove the typed-id forms from the purchases page
- [ ] T19: tests mirroring N2
- [ ] T20: regenerate the stylesheet

## Slice N4 — line entry
- [ ] T21: product search endpoint matching name, SKU and barcode, read-only and bounded
- [ ] T22: picker field with debounce and a results fragment, on both record pages
- [ ] T23: submit resolution order — barcode, then SKU, then id — so an exact barcode is one step
- [ ] T24: the response returns the updated lines, an out-of-band empty focused picker and the running
      total, so the loop needs no cart script
- [ ] T25: tests for AC8 to AC12, including a whole sale loaded without touching the mouse
- [ ] T26: regenerate the stylesheet

## Slice N5 — remaining names, filters and search
- [ ] T27: resolve names in the fragments not covered by the record pages: lists, statements, receipts
- [ ] T28: filters for sales and purchases: status, party, number, date
- [ ] T29: product filters for name, SKU and barcode, keeping the category filter
- [ ] T30: tests for AC13 and AC14, plus a guard assertion that no rendered page shows a bare entity id
- [ ] T31: README updated for the record pages, the picker and the filters

## Slice N6 — finish the shell conversion (cosmetic, last)
- [ ] T32: convert the remaining templates to the page-header pattern
- [ ] T33: remove the typed-id rule from the wiring guard, since no page requires a typed id any more, and
      confirm the guard still catches the mutations it used to
- [ ] T34: regenerate the stylesheet

## Verify
- [ ] `cargo test` green at the end of every slice, the wiring guard passing over every page, the
      stylesheet regenerated in every slice that adds classes, and a manual pass at 1024 px, 768 px and
      360 px widths
- [ ] Confirm no `alert()` remains and that a deliberately failed action renders a dismissible notice

## Archiving
- [ ] On merge: fold the interface requirements into the canonical specs as a presentation section, note
      the record-page routes in the `sales` and `purchases` capabilities, and move this change to
      `openspec/changes/archive/`.
