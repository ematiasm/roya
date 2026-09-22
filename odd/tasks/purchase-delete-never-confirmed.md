# Feature: Delete never-confirmed cancelled purchases

## Objective

Allow deleting purchases that were cancelled while still Draft (never
confirmed): they never touched stock or finance and are garbage rows.
Purchases cancelled AFTER confirmation stay permanently protected (they are
part of the audit trail — reversed movements and refund transactions
reference them).

## Problem

`delete_draft` only matches `status = 'Draft'`. "Discard draft" moves a
purchase to `Cancelled`, so discarded drafts accumulate forever with no way to
remove them — even though they posted nothing.

## Why

User asked "se pueden borrar los draft cancelados?" (2026-09-22) and approved
the split: delete cancelled-never-confirmed only; keep
cancelled-after-confirmed immutable.

## Key domain fact

`purchase_number` is assigned at confirm and is immutable/unique; it is NULL
exactly for never-confirmed purchases (REST test name: "null_only_draft").
Therefore:

- Cancelled + `purchase_number IS NULL` → was discarded as a draft → safe to
  delete.
- Cancelled + `purchase_number IS NOT NULL` → was confirmed then cancelled →
  protected forever.

## Authorized scope

- `src/repositories/purchase_repo.rs` — extend the DELETE predicate to
  `status = 'Draft' OR (status = 'Cancelled' AND purchase_number IS NULL)`;
  update doc comment; adjust/extend repo tests (see checklist).
- `src/services/purchases.rs` — `delete_draft` semantics/error copy for the
  newly refused case (cancelled-after-confirmed) must still name the state;
  service tests updated.
- `src/routes/purchases_web.rs` — same `DELETE /web/purchases/{id}` route and
  `PurchasesCreate` gate; update route tests if assertions pin old behavior.
- `templates/partials/purchase_detail.html` (+ any template that already
  renders a purchase draft-delete control, grep for the delete trigger) —
  show Delete for Cancelled records **only when `purchase_number` is None**,
  with confirm prompt; Confirmed and cancelled-after-confirmed get no button.
- Tests: `src/smoke_tests.rs` (grep-only) + e2e only if a DOM contract they
  pin changes.

## Out of scope

- Sales have the identical `delete_draft` pattern — NOT touched (separate
  request if wanted).
- REST API delete behavior: out of scope unless a web-only predicate change
  would make REST inconsistent — if REST exposes a purchase delete, mirror
  the same predicate there and note it; otherwise leave REST alone.
  **Resolved (T1): REST does NOT expose a purchase delete** —
  `src/routes/purchases_api.rs` registers
  `/api/purchases/{id}` as `get(get_purchase).put(update_purchase)` only, so
  there is no REST delete to keep consistent. REST left untouched.
- No push/PR unless user asks.

## Locked design decisions

1. Deletion predicate: `Draft OR (Cancelled AND purchase_number IS NULL)`.
2. Service refuses cancelled-after-confirm with a Validation naming the
   state (never silent).
3. UI: Delete button on the purchase record for Cancelled+no-number only;
   `hx-confirm` prompt; existing HX-Trigger `purchase-changed` refresh.
4. Ids/naming: keep the route and `HX-Trigger` contract unchanged; the
   repository method may keep the name `delete_draft` with an updated doc
   comment (renaming the trait ripples into sales — not authorized) OR be
   renamed purchase-side only if it does not touch the shared trait; prefer
   the least-churn option that stays honest in comments.
5. Lines are deleted with the purchase (existing behavior).

## Constraints

- Strict TDD ON (`openspec/config.yaml`), runner `cargo test`.
- Conventional commits, English artifacts, no AI attribution, RDD OFF.
- Preserve existing protected behavior with an explicit test:
  cancelled-AFTER-confirm must still refuse (existing test
  `delete_draft_on_a_cancelled_purchase_returns_false_and_the_row_survives`
  must be re-pinned to a fixture that was confirmed-then-cancelled, and a NEW
  test must pin that discarded (never-confirmed) cancelled purchases delete).

## Delivery strategy

- `ask-on-risk` + `stacked-to-main` (session cache). Aggregate 696 lines >
  400 → sliced into 3 PRs, each under budget (recorded at push, 2026-09-22):

  | PR | Branch | Commits | Increment | Lines |
  |----|--------|---------|-----------|-------|
  | (pending) | `feat/cancelled-delete-s1` | `8708c87` | T1 repo predicate | 267 |
  | (pending) | `feat/cancelled-delete-s2` | `f6789f9` | T2 service + route | 180 |
  | (pending) | `feat/purchase-delete-never-confirmed` | `69ae397, 36e88fe` + doc | T3 UI + T4 evidence | 277 + doc commits |

- Skill: `work-unit-commits`
  (`/home/mamull/.config/opencode/skills/work-unit-commits/SKILL.md`).

## Checklist

- [x] T1 Repo predicate + tests: discarded-cancelled deletes; protected test
      re-pinned to confirmed-then-cancelled; unknown-id unchanged.
      Commit: 8708c87.
      Evidence: RED `cargo test delete_draft` →
      `delete_draft_on_a_discarded_cancelled_purchase_deletes_it_with_its_lines`
      failed ("a never-confirmed cancelled purchase is deletable"), 20 passed;
      GREEN after predicate change → `cargo test delete_draft` = 21 passed;
      `cargo test repositories::purchase_repo` = 13 passed.
- [x] T2 Service + route behavior and error copy; tests RED→GREEN.
      Commit: f6789f9.
      Evidence: RED `cargo test delete_draft` → 2 failed, 23 passed
      (service `delete_draft_removes_a_discarded_cancelled_purchase...` and
      route `web_delete_draft_discarded_cancelled_purchase_answers_200_and_is_gone`,
      both with `Validation("purchase 1 is Cancelled: only a draft can be deleted")`
      / 400-vs-200); GREEN → 25 passed. Also `cargo test services::purchases`
      = 52 passed, `cargo test routes::purchases_web` = 55 passed. Refusal
      copy now: `purchase {id} is {state}: only a draft or a discarded
      (never-confirmed) cancelled purchase can be deleted` — names the state
      for Confirmed and for confirmed-then-cancelled.
- [x] T3 UI: Delete control on Cancelled+no-number record with confirm
      prompt; smoke/e2e assertions as needed.
      Commit: 69ae397.
      Evidence: RED `cargo test offers_delete` → 2 failed, 0 passed
      (`web_purchase_record_offers_delete_only_for_a_discarded_cancelled_purchase`
      "a discarded purchase must offer delete"; drawer test
      `document_drawer_discarded_purchase_offers_delete_but_annulled_does_not`
      no hx-delete rendered); GREEN → 2 passed. Regressions:
      `cargo test routes::documents_web` = 9 passed,
      `cargo test routes::purchases_web` = 56 passed,
      `cargo test drawer` = 25 passed, `cargo test purchase_record` = 16
      passed. Surfaces: `purchase_detail.html` (Cancelled + no number →
      Delete with hx-confirm, swap clears `#purchase-record`) and the
      documents drawer `purchase_actions` (Spanish "Eliminar descarte"
      with hx-confirm). Confirmed / cancelled-after-confirmed render no
      delete on either surface. Smoke tests untouched (grep-only: no
      pinned assertion covers the new state; fixtures are draft/confirmed).
- [x] T4 Full verification: `cargo test`; e2e if DOM contracts changed;
      evidence per task.
      Evidence: full `cargo test` = **822 passed, 0 failed** (78.78s) on
      this branch after T1–T3. E2E: **skipped — no e2e-pinned DOM contract
      changed** (the browser suite pins the SALE draft drawer delete, the
      confirmed purchase record, the picker and the sales discard dialog —
      all untouched; the new Delete controls render only for discarded
      purchases, a state no browser test visits; per e2e/README the suite
      runs only when a pinned contract changes).
      Work-unit commits: T1 8708c87 (repo predicate), T2 f6789f9
      (service + route), T3 69ae397 (UI), T4 this doc-evidence commit.

## Route declaration

T1–T3: delegated direct (writer trigger: 2+ non-trivial files).

## Progress

- [x] All tasks complete. Next step: none (awaiting review; no push/PR
      per locked scope).
  - T1 8708c87 — repo predicate `Draft OR (Cancelled AND purchase_number
    IS NULL)`; protected test re-pinned to confirmed-then-cancelled; new
    discarded-delete test. REST check: `/api/purchases/{id}` has no DELETE
    (get/put only) → REST untouched.
  - T2 f6789f9 — service `delete_draft` admits the discarded state and
    refuses the rest with a Validation naming the state; route tests for
    both paths.
  - T3 69ae397 — record page Delete (Cancelled+no number, hx-confirm) and
    documents-drawer "Eliminar descarte"; both pinned present/absent.
  - T4 — full `cargo test` 822/822 green; e2e skipped (no pinned DOM
    contract changed).

## Acceptance criteria

1. Discarded (never-confirmed) cancelled purchase: DELETE removes it and its
   lines; record page 404s afterwards.
2. Confirmed-then-cancelled purchase: DELETE refuses with Validation naming
   the state; row and lines survive.
3. Draft delete still works exactly as before; Confirmed still 400.
4. UI shows Delete only for Draft (existing) and Cancelled-without-number;
   never for Confirmed or cancelled-after-confirmed.
5. `cargo test` green; affected e2e green if touched.
