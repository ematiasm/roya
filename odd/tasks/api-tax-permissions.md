# Close the tax definition permission gap in the JSON API

## Objective

Make the JSON API agree with the web surface: a principal holding only `inventory.write` can no longer create, rename, re-rate, or activate/deactivate a tax definition. Definition administration becomes `settings.manage` on every surface. Read access and product-tax association stay where they are.

## Problem

U1 removed the Products tax catalogue, so on the web no `inventory.write` route can administer a tax definition. The JSON API still can:

- `POST /api/taxes` → `create_tax`, `Require<InventoryWrite>` (`src/routes/inventory_api.rs`)
- `PUT /api/taxes/{id}` → `update_tax`, `Require<InventoryWrite>`
- `POST /api/taxes/{id}/deactivate` → `deactivate_tax`, `Require<InventoryWrite>`

So the same principal has fewer powers in a browser than in a script. That is a real authorization inconsistency, and it was recorded as an explicit open decision in `odd/tasks/product-price-ladder.md` rather than left as an accident.

## Why

- A permission boundary is only a boundary if it holds on every surface. Half-enforced authorization is worse than a clearly un-enforced one, because the UI implies a rule the API does not keep.
- Tax definitions are configuration, not inventory. This feature already established that split for the web surface; the API is the remaining half.

## Decisions

- **Definition mutations move to `SettingsManage`:** `POST /api/taxes`, `PUT /api/taxes/{id}`, and `POST /api/taxes/{id}/deactivate`.
- **Reads stay on `InventoryRead`:** `GET /api/taxes` and `GET /api/taxes/{id}` do not change. Reading a rate is not configuration, and inventory surfaces need the catalogue to render pickers and previews. The product-tax association's own read, `GET /api/products/{id}/taxes`, is on `InventoryRead` too and is likewise unchanged — it is a read of inventory state, not a tax-definition read, and it is listed here so nobody reads "association stays on `InventoryWrite`" as covering the whole path.
- **Association MUTATIONS stay on `InventoryWrite`:** `POST /api/products/{id}/taxes` and `DELETE /api/products/{id}/taxes/{tax_id}` do not change. Linking a tax to a product is inventory work, not defining a tax. Stated precisely because the path is not uniform: an `inventory.write`-only principal may link and unlink, and is **refused** `GET /api/products/{id}/taxes` (403) and `GET /api/taxes` (403), because both reads are `inventory.read`. "Write" never implied "read back" on this surface, before or after this task.
- **The `settings.manage` side is asymmetric on purpose, and it is a consequence of "reads stay on `InventoryRead`", not a defect.** A principal holding `settings.manage` and nothing else can perform all five definition mutations through the API, and still cannot `GET /api/taxes`, `GET /api/taxes/{id}` or `GET /api/products/{id}/taxes`, nor link a tax to a product. So a settings-only JSON client can write a definition it cannot list. That trade is accepted on purpose: keeping the catalogue readable by `inventory.read` is what lets inventory surfaces render pickers and previews, and narrowing it to `settings.manage` was explicitly out of scope. The web Settings tab is unaffected, because it renders through `TaxService` behind its own `settings.manage` page gate and never passes through the JSON gate. A client that needs both halves therefore needs both permissions, which is the pre-existing shape of this API and not something this task changed.
- **Hard delete remains exclusive to Settings** and has no JSON route at all; this is unchanged.
- The seeded protected administrator already holds `settings.manage`, so the application itself is not locked out.
- Any consumer that was relying on an `inventory.write` client creating taxes will now get 403. That is the intended effect and must be stated, not quietly absorbed.
- Technical artifacts, code, comments, tests, and UI copy are English.

## Authorized scope

Repository-local route, test, documentation, and feature-document changes. No push, PR creation, or merge without explicit user request. No database reset and no change to the development database. No change to tax calculation, pricing, or presentation behavior.

## Route declaration

One bounded delegated-direct ODD work unit, because it touches several non-trivial files: three route gates, the tests that pin them, the browser fixtures that consume the API, and the seven places of prose that currently declare the residual open. The parent owns task closure, the work-unit commit, verification, and delivery reporting. No SDD phase is created.

## TDD mode

- Effective mode: strict TDD inherited from project configuration (`openspec/config.yaml`, `strict_tdd: true`).
- Test runner: `cargo test`.
- Observe RED before the gate change, then GREEN, then a refactor/check pass.
- The RED must be behavioral: a request that currently succeeds under `inventory.write` and must now be refused.

## Work units

- [x] U1 — Narrow the three JSON tax-definition gates to `SettingsManage` and close the recorded residual.
  - Three gates changed in `src/routes/inventory_api.rs`: `create_tax`, `update_tax` and `deactivate_tax`, each `Require<InventoryWrite>` → `Require<SettingsManage>`. Nothing else moved: no fourth gate, and no gate weakened.
  - Reads stayed on `InventoryRead`, including `GET /api/products/{id}/taxes`, which was already `InventoryRead` and is refused to an `inventory.write`-only principal. Association **mutations** stayed on `InventoryWrite`.
  - `tax_api_preserves_inventory_permission_boundaries` was replaced by two tests: one that proves `inventory.write` is refused on all three definition mutations and writes nothing, and one that proves `settings.manage` still performs every definition mutation through the same router. The positive half matters — without it a 403 could be ambient rather than discriminating.
  - Consumer audit found **zero** in-repo consumers to move: `inventory.write` appears nowhere in `e2e/`, every JSON tax call runs as the first-run protected administrator, and migration `20240101000037` grants that role `settings.manage`. No assertion was weakened.
  - Eight places of open-decision prose were closed with dated closing lines while the historical T3 and U1 records keep what was true at the time. Independent verification then found the test docstring still made a false historical claim about what the replaced test pinned; that was corrected, and the writer found a fourth instance of the same falsehood independently.
  - Evidence: strict TDD RED was behavioral — `POST /api/taxes` answered `201 Created` with the created tax under `inventory.write` (`left: 201, right: 403`). Final `cargo test tax_api_` → 3 passed, `cargo test tax_` → 100 passed, `cargo test product` → 111 passed, `cargo test settings_` → 25 passed, `cargo test` → 1096 passed, `cargo check --all-targets` → 0 errors with a warning baseline byte-identical to the stashed pre-change state, `cargo fmt --check` and `git diff --check` clean, `bash scripts/e2e.sh tests/test_settings.py` → 6 passed, `tests/test_products.py` → 24 passed / 1 pre-existing opt-in skip, `tests/test_visual_baseline.py` → 1 passed in verify mode with no regeneration because no template or static file was touched. Independent verification returned PASS after one correction round and confirmed the change is a strict permission swap that breaks no in-repo consumer. Parent spot check repeated `cargo test tax_api_` → 3 passed. Commit identity is recorded in this document after the work-unit commit.

## Acceptance criteria

1. `POST /api/taxes` with only `inventory.write` is refused and writes nothing.
2. `PUT /api/taxes/{id}` with only `inventory.write` is refused and changes nothing.
3. `POST /api/taxes/{id}/deactivate` with only `inventory.write` is refused and changes nothing.
4. A `settings.manage` principal can still create, rename, re-rate, activate and deactivate a tax definition through the API. It cannot read the catalogue back through JSON, and cannot link a tax to a product; that asymmetry is the accepted consequence of criterion 5, and the web Settings tab is unaffected.
5. `GET /api/taxes` and `GET /api/taxes/{id}` still work for `inventory.read`, unchanged.
6. `/api/products/{id}/taxes` and `/api/products/{id}/taxes/{tax_id}` still work for `inventory.write`, unchanged — for the MUTATIONS. Precisely: `POST` and `DELETE` on those paths answer for `inventory.write`, while `GET /api/products/{id}/taxes` is `InventoryRead` and is refused to an `inventory.write`-only principal, exactly as before this task.
7. No JSON route can hard-delete a tax.
8. No documentation, comment, or test name still claims the API gap is open.
9. The protected administrator retains working tax administration end to end.
10. Focused tests, the full Rust suite, and the applicable browser checks pass, with all skips recorded.

## Applicable checks

- `cargo test tax_`
- `cargo test product`
- `cargo test settings_`
- `cargo test`
- `cargo check --all-targets`
- `cargo fmt --check`
- `bash scripts/e2e.sh tests/test_settings.py`
- `bash scripts/e2e.sh tests/test_products.py`
- `bash scripts/e2e.sh tests/test_visual_baseline.py`
- `git diff --check`

## Progress and evidence

- Baseline: branch `feat/tax-calculation-settings` at `f5cf904`, working tree clean.
- Verified residual: the three JSON handlers are `Require<InventoryWrite>`. The old `tax_api_preserves_inventory_permission_boundaries` did **not** pin that admission — it only proved the three endpoints were refused an `inventory.read`-only principal, and no test in the repository ever asserted an `inventory.write` success on them. The residual was therefore pinned by the three gate declarations and by the prose that recorded it as an open decision, not by a test, which is why U1's RED required a **new** assertion rather than an inverted one.
- Feature document: `odd/tasks/api-tax-permissions.md`.
- Engram mirror topic: `odd/api-tax-permissions/tasks`.
- Verification: PASS. The only behavioral change is three permission gates, proven independently non-vacuous against each gate individually, with reads and association provably unmoved and no assertion, template or route touched.
- Delivery: work-unit commit `796a6c1` (`fix(auth): make tax definition administration settings-only on every surface`).
- Next step: delivery is the user's decision. No push, PR creation, or merge has been performed.

### Staging note

`odd/tasks/pos-counter-sales.md` is untracked and belongs to a different task. It must NOT be staged by this work unit. Stage the seven modified files and `odd/tasks/api-tax-permissions.md` explicitly.
