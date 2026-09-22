# Feature: Purchase payment decided at confirm

## Objective

Remove every payment decision from draft surfaces: Create Draft asks only
supplier + purchase date; the Sugerido seed asks only purchase date; payment
type (Cash/Credit) and due date move into the Confirm dialog; the payment-type
badge and effects preview stop asserting a type while the purchase is a Draft.

## Problem

After the receiving-desk redesign, Create Draft still showed `Type`
(Cash/Credit) and `Due date`, and the Sugerido seed options showed the same.
User principle: payment is imputed at confirm, so Contado/Crédito and due date
ARE payment decisions and do not belong on a draft.

## Why

User reviewed the shipped receiving desk and corrected the scope boundary:
locked decision #2 covered the payment *method* selector, but the *type* and
due date were left on creation. Follow-up request 2026-09-22: "create draft
solo con proveedor y fecha".

## Authorized scope

- `templates/purchases.html` — Create Draft form: ONLY supplier + purchase
  date (remove Type, Due date, invoice, notes inputs).
- `templates/partials/suggestion_list.html` — seed options: ONLY purchase
  date (remove Type + Due date).
- `templates/partials/purchase_detail.html` — Confirm dialog gains payment
  type (Cash/Credit) + due date (Credit only) + method (Cash, existing);
  Edit header drops the Due date field (keeps purchase date, invoice, notes);
  hide payment-type badge while Draft.
- `templates/partials/purchase_list.html` — hide payment-type badge for Draft
  rows.
- `templates/partials/purchase_effects_preview.html` — do not branch on
  stored type: show both scenarios until confirm (Cash: −total at confirm /
  Credit: due date chosen at confirm).
- `src/routes/purchases_web.rs` — create route defaults `payment_type` to
  Cash when the form omits it; `from-suggestion` seed same default; confirm
  route accepts `payment_type` + `due_date` (+ existing `method_id`),
  updates the draft header, then confirms (reusing the existing header-update
  semantics, including due-date clearing for Cash).
- Tests: `src/smoke_tests.rs` (grep-only) + `e2e/tests/test_purchases.py`,
  `test_picker.py`, `test_confirmation.py` as DOM contracts change.

## Out of scope

- REST `POST /api/purchases` keeps accepting explicit `payment_type`
  (separate interface; not requested).
- Payment-type badge remains visible for Confirmed/Cancelled purchases.
- Sales flows untouched.
- No push/PR unless user asks.

## Locked design decisions (this feature)

1. Create Draft form: supplier + purchase date ONLY.
2. Sugerido seed: purchase date ONLY.
3. Confirm dialog: radio Cash/Credit (prefilled from stored value — REST or
   legacy drafts may already carry a type), due date field only when Credit,
   method select only when Cash (already exists).
4. Draft records default to Cash server-side when the form omits the type;
   the stored default is never displayed while Draft (badges hidden).
5. Edit header: remove Due date; keep purchase date, invoice, notes.
6. Effects preview: both scenarios (no stored-type branching).
7. Confirm route order: validate/update header (type, due; clear due for
   Cash) then call existing confirm; failures leave the draft updated but
   unconfirmed — pinned by test.

## Constraints

- Strict TDD ON (`openspec/config.yaml`), runner `cargo test`.
- Server remains authoritative (service confirm rules unchanged).
- Keep legacy ids (#confirm-purchase, #edit-header, etc.).
- RDD OFF; conventional commits; no AI attribution; English artifacts.

## Delivery strategy

- Inherited session choices: `ask-on-risk`, chain `stacked-to-main` if
  forecast > 400; `size:exception` only with explicit maintainer approval.
- Skill: `work-unit-commits`
  (`/home/mamull/.config/opencode/skills/work-unit-commits/SKILL.md`).

## Checklist

- [x] T1 Create Draft + Sugerido seed: only supplier/date fields; routes
      default Cash; RED tests for absent inputs.
      Evidence: `cargo test web_create_draft_form_asks_only_supplier_and_purchase_date`
      → RED `test result: FAILED. 0 passed; 1 failed` (form still asked
      `payment_type`), GREEN after template edits `1 passed`.
      `cargo test payment_type_defaults_to_cash` → `2 passed` (pinning the
      pre-existing server default, green immediately). `cargo test purchases_web`
      → `46 passed, 762 filtered out`.
- [ ] T2 Confirm dialog gains type + due (prefilled); confirm route updates
      then confirms; RED tests for Cash/Credit paths incl. due clearing.
- [ ] T3 Hide Draft type badges (detail + list); effects preview shows both
      scenarios; RED tests.
- [ ] T4 Edit header drops Due date field; RED test.
- [ ] T5 Verification: full `cargo test`; e2e purchase suites; evidence per
      task in this doc.

## Route declaration

T1–T4: delegated direct (writer trigger: 2+ non-trivial files).

## Progress

- [ ] All tasks pending. Next step: T1.

## Acceptance criteria

1. Create Draft form contains exactly: supplier, purchase date (+ submit).
   No Type/Due/invoice/notes inputs.
2. Sugerido seed options: purchase date only.
3. Draft rows/cards show NO payment-type badge; Confirmed/Cancelled still do.
4. Effects preview does not depend on stored type; shows cash and credit
   scenarios.
5. Confirm dialog: type radio prefilled from stored value; Credit shows due
   date input (required); Cash shows method (required) and posts due=empty.
6. Successful Cash confirm leaves purchase with no due date; successful
   Credit confirm persists the chosen due date (service rule).
7. Edit header has no Due date field; purchase date/invoice/notes remain.
8. `cargo test` green; affected e2e green.
