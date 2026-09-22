# Feature: Delete never-confirmed cancelled sales (parity with purchases)

## Objective
Mirror the purchases fix (PRs #86–#88): let deleting a **discarded** cancelled sale — one
cancelled while still Draft, never confirmed — while confirmed-then-cancelled sales stay
permanently protected.

## Problem
`SaleRepository::delete_draft` and `SaleService::delete_draft` only admit `status = 'Draft'`.
"Discard draft" on sales moves the row to `Cancelled` (`sale_number` still NULL), so discarded
sales accumulate forever with no removal path, even though they posted nothing (no stock, no
payment, no ledger, no debt — the repo doc comment already states a draft is the only deletable
state *by construction*; a never-confirmed Cancelled row has the same property).

## Why
User request after the purchases parity gap was surfaced during review of
`purchase-delete-never-confirmed` (2026-09-22): "dale hacelo". The pattern and tests are fresh
from purchases; this is the deliberate mirror, not scope creep.

## Key domain fact
`sale_number` is assigned at confirm (immutable), NULL exactly for never-confirmed sales —
identical to `purchase_number`. Cancelled + NULL number = discarded draft = safe to delete.
Cancelled + number = confirmed-then-cancelled = permanent audit trail (refund transactions
reference its payments).

## Authorized scope
- `src/repositories/sale_repo.rs` — `delete_draft` predicate →
  `status = 'Draft' OR (status = 'Cancelled' AND sale_number IS NULL)`; trait doc comment;
  repo tests (re-pin cancelled-protection test to confirmed-then-cancelled fixture; new
  discarded-cancelled test).
- `src/services/sales.rs` — `delete_draft` deletable check + refusal copy naming the state for
  confirmed-then-cancelled; service tests.
- `src/routes/sales_web.rs` — `DELETE /web/sales/{id}` route doc + route tests (discard-200,
  annulled-400, unknown-id) mirroring the purchase route tests.
- `src/routes/documents_web.rs` — `sale_actions` Cancelled arm: offer Delete only when
  `sale_number.is_none()` && `PurchasesCreate`-equivalent permission (`SalesCreate`); drawer
  test mirroring `document_drawer_discarded_purchase_offers_delete_but_annulled_does_not`.
- `templates/` — sale record surface: Delete control for Cancelled + `sale_number` None only
  (check where the sale draft delete currently renders; mirror `purchase_detail.html` shape).
- `src/smoke_tests.rs` grep only.

## Out of scope
- Any REST change (verify: sales API must not grow a DELETE; if one exists, mirror the web
  predicate decision — currently expected absent, confirm during T1).
- Customer receipts, annulment flows, purchases (already done).
- No push/PR unless asked (session cache: user asks explicitly).

## Locked design decisions
1. Predicate: `Draft OR (Cancelled AND sale_number IS NULL)` — exact mirror of purchases.
2. Service refuses confirmed-then-cancelled with Validation naming the state.
3. UI: Delete for Draft (existing) and Cancelled-without-number only; `hx-confirm` prompt;
   keep existing `HX-Trigger: sale-changed` contract.
4. Keep method name `delete_draft` (same least-churn choice as purchases).
5. Lines deleted with sale (CASCADE, existing behavior).
6. Re-pin the existing cancelled-protection test (if its fixture is a discarded draft) to a
   confirmed-then-cancelled fixture; add a NEW discarded-cancelled test.

## Constraints
- Strict TDD ON (`openspec/config.yaml`, runner `cargo test`): RED → GREEN → REFACTOR per task.
- Conventional Commits, no AI attribution, English artifacts.
- RDD OFF → verification = `gentle-ai review assess` + parent spot check + independent verifier.
- `src/smoke_tests.rs`: grep only, never read wholesale (334K).
- Keep existing sale ids/DOM contracts pinned by e2e.

## Delivery strategy
`ask-on-risk` + `stacked-to-main` (session cache). Aggregate 601 lines > 400 →
sliced into 2 PRs, each under budget (recorded at push, 2026-09-22):

| PR | Branch | Commits | Increment | Lines |
|----|--------|---------|-----------|-------|
| [#89](https://github.com/ematiasm/roya/pull/89) | `feat/sale-delete-s1` | `83f2e4e, df59bc5, b0ba110, 311c930` | plan + T1 repo + T2 service/route | 391 |
| [#90](https://github.com/ematiasm/roya/pull/90) | `feat/sale-delete-never-confirmed` | `bc1f454, 24a7ecd` + doc commits | T3 UI + T4 evidence | 219 |

Skill: `work-unit-commits`
(`/home/mamull/.config/opencode/skills/work-unit-commits/SKILL.md`).

## Route declaration
T1–T3: **delegated direct** (writer trigger: 2+ non-trivial files).

## Checklist
- [x] T1 Repo predicate + tests (discarded deletes; protection re-pinned to
      confirmed-then-cancelled; unknown-id unchanged; REST DELETE absence confirmed).
      Commit: df59bc5.
- [x] T2 Service + route (RED→GREEN; refusal names state; contract unchanged).
      Commits: b0ba110 + 311c930 (route tests).
- [x] T3 UI both surfaces (record page + documents drawer) + tests; smoke/e2e if pinned
      contracts change.
      Commit: bc1f454.
      Evidence: RED `cargo test web_sale_record_offers_delete_only` → 1 failed
      ("a discarded sale must offer delete") and `cargo test
      document_drawer_discarded_sale_offers` → 1 failed ("a discarded sale
      must offer its delete"); GREEN after the sale_actions Cancelled arm
      (Delete iff `sale_number.is_none()` && `SalesCreate`) and the
      sale_detail.html Cancelled+None button (hx-confirm, `HX-Trigger:
      sale-changed` contract kept). The old
      `document_drawer_cancelled_sale_offers_no_action` was renamed
      `..._confirmed_then_cancelled_...` — its fixture confirms first, so the
      expectation holds; the name now tells the truth. Record fixture got the
      purchases-style per-process SKU/account suffix (two fixtures per test
      collided on `REC-P1`/`Caja`). Smoke: `cargo test smoke` → 93 passed
      (grep: no pinned assertion covers the new discarded state; fixtures are
      draft/confirmed). E2E: skipped — no e2e-pinned DOM contract changed
      (the suite pins the SALE draft drawer delete, the discard/cancel
      dialogs and the cancelled list row — all untouched; the new Delete
      buttons render on discarded-sale surfaces, and no browser test asserts
      their absence).
- [x] T4 Full `cargo test` green; e2e if affected; evidence checkoffs with commit hashes.
      Evidence: full `cargo test` = **830 passed, 0 failed** (74.19s) after
      T1–T3. E2E: **skipped — no e2e-pinned DOM contract changed** (the
      browser suite pins the SALE draft drawer delete, the discard/cancel
      dialogs, and the cancelled list row — all untouched; the new Delete
      controls render only on discarded-sale surfaces, which no browser test
      asserts to be delete-free; per e2e/README the suite runs only when a
      pinned contract changes). Work-unit commits: T1 df59bc5, T2 b0ba110 +
      311c930, T3 bc1f454, T4 this doc-evidence commit.

## Progress

- [x] All tasks complete. Next step: none (awaiting review; no push/PR per
      locked scope).
- [x] T1 done — RED (822 passed / 1 failed: discarded) then GREEN via predicate + trait-doc (delete_draft: Draft OR Cancelled∧sale_number IS NULL). Commit: df59bc5. REST check: `/api/sales/{id}` has no DELETE (get/put only; line-level DELETE pre-exists on `/api/sales/lines/{line_id}` but no sale-document DELETE) → REST untouched.
- [x] T2 done — service deletable check (Draft OR Cancelled∧number NULL), refusal + race copy mirrored from purchases, doc comment + route doc updated; 2 new service tests (discarded removes, confirmed-then-cancelled refuses). Commits: b0ba110 (service + route doc), 311c930 (route tests: discard-200, confirmed-then-cancelled-400, unknown-404 — GREEN immediately, pinning behavior landed in b0ba110; RED was observed at the service layer in the T2 work).
- [x] T3 done — record page Delete (Cancelled+no number, hx-confirm) and documents-drawer "Eliminar descarte" (SalesCreate-gated); both pinned present/absent; old cancelled-offers-no-action test renamed to confirmed_then_cancelled (fixture confirms first — expectation unchanged); record fixture got purchases-style per-process SKU/account suffix. Commit: bc1f454. Evidence: RED (both new tests failed "must offer delete") → GREEN. Smoke `cargo test smoke` → 93 passed; e2e skipped (no pinned contract changed — see checklist).
- [x] T4 done — full `cargo test` → **830 passed, 0 failed** (74.19s) on this branch after T1–T3. Work-unit commits: T1 df59bc5, T2 b0ba110 + 311c930, T3 bc1f454, T4 this doc-evidence commit.

## Acceptance criteria
1. Discarded cancelled sale: DELETE removes it + lines; record page 404s after.
2. Confirmed-then-cancelled sale: DELETE refuses Validation naming the state; row survives.
3. Draft delete unchanged; Confirmed still 400 (existing tests stay green).
4. UI: Delete offered only for Draft (existing) and Cancelled-without-number — never for
   Confirmed or cancelled-after-confirmed; both surfaces covered.
5. `cargo test` green; affected e2e green if any pinned contract changed.
