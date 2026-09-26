# Tax calculation and administration

## Objective

Calculate tax-inclusive product and document amounts from net product prices, preserve the exact tax facts used by each sale or purchase line, and add a dedicated Taxes tab to Settings for tax creation, editing, and safe hard deletion.

## Problem

The application already stores tax definitions and many-to-many product associations, but tax administration lives on the Products page and sale and purchase totals ignore taxes. Sales and purchases also do not snapshot tax identity, rate, or amount, so changing a tax cannot preserve the historical meaning of a document. There is no application-level hard-delete operation: deactivation exists, and the database only backstops product associations with `ON DELETE RESTRICT`.

## Why

Taxes must affect the financial totals, not merely appear as product metadata. Historical documents must remain reproducible after a tax is edited, and Settings needs an explicit tax-management surface. A hard delete must be rejected when either current product associations or immutable document snapshots still reference the tax.

## Scope

### Calculation and snapshots

- Treat product sale price and purchase unit cost as net amounts.
- Resolve all active taxes linked to the product at the time a document line is created.
- Sum linked tax rates independently; taxes do not compound.
- Calculate each line as `net subtotal + tax amount` and round the tax-inclusive line total to the currency's two decimal places using half-up rounding.
- Snapshot the tax code, name, rate, and calculated contribution on each document line so later catalog edits do not rewrite history.
- Include snapshot tax totals in sale and purchase line, document, payment-limit, and debt calculations consistently.
- Show net subtotal, tax breakdown, tax total, and final total in document detail views.
- Show a net-price, linked-tax, and tax-inclusive-price summary for products without changing the stored net sale price.

### Settings tax administration

- Add a Taxes tab to `/settings`, gated by `SettingsManage`.
- Allow authorized users to create, edit, activate/deactivate, and hard-delete taxes from the tab.
- Preserve decimal input and localized percentage/currency presentation.
- Reject duplicate tax codes through the existing service and database uniqueness contract.

### Deletion safeguard

- Add an application-level hard-delete operation.
- Reject deletion with an actionable conflict when any `product_taxes` row references the tax.
- Reject deletion with an actionable conflict when any sale-line or purchase-line tax snapshot references the tax.
- Permit deletion only when no current product association and no document snapshot reference the tax.
- Retain foreign-key `RESTRICT` constraints as the database backstop.

## Constraints

- Canonical decimal values remain SQLite `TEXT` and Rust `Decimal`; localized display values are never persisted.
- Product prices and purchase costs are net under this feature.
- Multiple linked taxes are additive, not compound.
- Existing confirmed document totals, paid amounts, and debt calculations must remain financially consistent after the change.
- Technical artifacts and code comments are English.
- No push, PR creation, or merge without explicit user request.
- Work-unit commits include their tests and use Conventional Commit messages.
- Receipt-driven development is currently disabled by the user's global setting; no native review is started by this feature.

## Authorized scope

Repository-local source, migration, test, template, static asset, and feature-document changes required to implement the behavior above. Database reset, push, PR creation, merge, and production data mutation are outside this feature.

## Route declaration

This is substantial delegated-direct ODD work. Each work unit touches multiple non-trivial files and therefore uses one bounded writer. The parent owns scope, task closure, work-unit commits, verification, and delivery reporting. No SDD phase is created.

## TDD mode

- Effective mode: strict TDD inherited from project configuration.
- Source: `openspec/config.yaml` / Engram `sdd-init/roya` reports `strict_tdd: true`.
- Test runner: `cargo test`.
- Each behavior work unit must observe RED before implementation, GREEN after implementation, and a refactor/check pass before closure.

## Work-unit forecast and delivery

- Forecast: approximately 500-750 authored changed lines across schema, calculation, routes, templates, tests, and E2E coverage.
- Delivery strategy: `ask-on-risk`; the user selected `stacked-to-main` after T1's actual size reached approximately 1,350 authored lines.
- Work units are reviewable commits. Delivery slices must be stacked to `main` in dependency order and remain independently understandable; no push or PR creation without explicit user authorization.
- About 400 authored lines is advisory planning guidance, not a reason to omit tests or compress code.

## Work units

- [x] T1 — Add immutable tax snapshot persistence and a shared line-tax calculation contract.
  - Added migration columns/tables and focused schema/repository tests.
  - Added model/repository support for product tax resolution and persisted snapshot values.
  - Added deterministic additive calculation with two-decimal half-up rounding.
  - Evidence: strict TDD RED was observed for each missing transaction, seam, and guard; final `cargo test tax_snapshot` → 38 passed, 967 filtered out; `cargo test tax_` → 47 passed; `cargo test` → 1005 passed; `cargo check --all-targets` → 0 errors, 82 warnings; `cargo fmt --check` and `git diff --check` clean. Independent verification found and closed non-atomic writes, Draft/Confirmed mutation seams, and uncovered rollback paths, then returned PASS. Parent spot check repeated `cargo test tax_snapshot` → 38 passed. Commit identity is recorded in this document after the work-unit commit.

- [x] T2 — Integrate tax snapshots and tax-inclusive totals into sales and purchases.
  - Document lines snapshot linked taxes through the atomic T1 repository contract.
  - Line/document totals, payment limits, debt calculations, customer receipts, supplier payments, document index, JSON API, and detail presentation now consume stored tax totals consistently.
  - Product presentation shows the stored net price, linked tax breakdown, and derived tax-inclusive price without mutating the net price.
  - Evidence: strict TDD RED proved document totals ignored stored tax; final `cargo test tax_snapshot` → 41 passed, `cargo test sales` → 122 passed, `cargo test purchases` → 161 passed, `cargo test documents` → 30 passed, `cargo test localization_tests` → 39 passed, `cargo test tax_` → 85 passed, `cargo test` → 1043 passed, `cargo check --all-targets` → 0 errors, 81 warnings, `cargo fmt --check` and `git diff --check` clean, and `bash scripts/e2e.sh tests/test_visual_baseline.py` → 1 passed. Independent financial verification audited every money path, found a vacuous locale guard, an intentional-but-stale visual baseline, and a duplicate product no-tax notice; all were closed and final independent verdict was PASS. Parent spot check repeated `cargo test tax_preview` → 8 passed. CSS build is N/A because every new template utility class already exists in committed `static/tailwind.css`. Commit identity is recorded in this document after the work-unit commit.

- [x] T3 — Add the Settings Taxes tab and hard-delete safeguard.
  - Added a localized `?tab=taxes` Settings tab with a dedicated `/web/settings/taxes…` surface under `SettingsManage`, while preserving the existing business settings form and the inventory product-drawer association surface.
  - Supports create, edit, activate/deactivate, and server-enforced confirmed hard delete.
  - Deletion is refused separately for product associations and document snapshots, with document history taking priority, and a raced foreign-key refusal is re-resolved into the same localized conflict.
  - Database and internal faults are logged and answered as 500; only genuine refusals keep 409/404/400 with actionable localized copy.
  - Evidence: strict TDD RED covered the missing tab, routes, service methods, and fault status; final `cargo test settings_` → 23 passed, `cargo test tax_` → 98 passed, `cargo test tax_snapshot` → 41 passed, `cargo test` → 1063 passed, `cargo check --all-targets` → 0 errors, 81 warnings, `cargo fmt --check` and `git diff --check` clean, `bash scripts/e2e.sh tests/test_settings.py` → 6 passed, and `bash scripts/e2e.sh tests/test_visual_baseline.py` → 1 passed without regeneration because `/settings` is not baselined. Independent verification found unlogged database faults, uncovered localized history-refusal copy, and an overstated authorization note; all were closed and final independent verdict was PASS. Parent spot check repeated `cargo test settings_` → 23 passed. CSS build is N/A because the new templates introduce no class missing from the committed stylesheet. Commit identity is recorded in this document after the work-unit commit.

### T3 design decisions

- **Canonical tax-administration URL surface.** Settings owns tax administration, so
  every tax mutation the Taxes tab issues is addressed under `/web/settings/taxes…`
  (`POST` create/edit/activate/deactivate/delete, `GET` list and delete-confirmation) and is
  gated by `settings.manage`, on the page at `/settings?tab=taxes`. The pre-existing
  `/web/taxes`, `/web/taxes/edit` and `/web/taxes/deactivate` routes are a DIFFERENT surface:
  they belong to the Products screen's tax catalogue, are gated by `inventory.write`, and the
  product drawer renders their fragments. They were left untouched on purpose.
- **The real authorization invariant (stated exactly).** This feature makes tax administration
  *irreversible* only under `settings.manage`: a hard delete — the one operation that destroys a
  definition instead of editing it — exists on no other route, and `inventory.write` cannot reach
  it. It does NOT make the catalogue `settings.manage`-only. A principal holding
  `inventory.write` can still create, rename, re-rate and deactivate/activate taxes from the
  Products screen's catalogue, exactly as before this feature, and that remains true. Both
  surfaces are therefore real, both keep their own permission, and they share no handler; merging
  them would either hand deletion to `inventory.write` or break the product page, its catalogue
  and their tests. Whether the catalogue's write access should eventually narrow to
  `settings.manage` is a separate authorization decision, not a side effect of T3.
  - **LATER, 2026-09-25 — that follow-up is decided and closed; the two bullets above are T3
    evidence, not a live claim.** U1 removed the Products catalogue, and then
    `odd/tasks/api-tax-permissions.md` narrowed the JSON API's `POST /api/taxes`,
    `PUT /api/taxes/{id}` and `POST /api/taxes/{id}/deactivate` to
    `Require<SettingsManage>`. Tax DEFINITION administration is `settings.manage` on every
    surface now. Reading a definition (`GET /api/taxes`, `GET /api/taxes/{id}`) is still
    `inventory.read`, the product-tax association is still `inventory.write` on both
    surfaces, and the hard delete is still `settings.manage` and still has no JSON route.
    The web claim this document makes is therefore true application-wide, and
    `tax_api_definition_administration_is_exclusive_to_settings_manage` in `src/tax_tests.rs`
    is the proof. Nothing in the T3 evidence above was rewritten.
- **Tab mechanism.** One `?tab=` query parameter on the existing `/settings` route, business tab
  as the default. The business form keeps its own URL, its own submission and its own markup; the
  taxes catalogue is read only when the taxes tab is selected.
- **Deletion conflict semantics.** Two reference families, never summed: a `product_taxes` link is
  removable current state (unlink the product), while a `sale_line_taxes` / `purchase_line_taxes`
  snapshot is frozen history with no remedy (keep the tax, deactivate it instead). When BOTH
  block the tax, the HISTORY conflict is the one reported: no amount of unlinking can free a tax a
  document already froze, so reporting the product link first would send the operator through work
  that cannot end in the deletion they asked for. Each reason has its own localized message.
- **Two-step delete.** The row's delete button only fetches a server-rendered confirmation panel
  naming the tax and both reference counts; the panel's form carries the `confirm` field and the
  delete route refuses a request without it, so the confirmation is enforced by the server and not
  only by the button.
- **No delete audit actor.** `taxes` has no delete audit trail (`created_by`/`updated_by` die with
  the row and the project has no generic audit log), so `TaxService::delete_tax` and
  `TaxRepository::hard_delete` deliberately take no actor parameter rather than advertising an
  audit that does not exist.
- **CSS build is N/A for this unit.** The three utility classes the new templates use that are
  absent from the committed `static/tailwind.css` (`self-end`,
  `sm:grid-cols-[100px_1fr_120px_auto]`, `whitespace-nowrap`) are the SAME classes the committed
  Products tax catalogue already uses, so T3 introduces no new styling requirement and the Taxes
  tab is guaranteed to render like the existing catalogue. The committed stylesheet is stale for
  14 classes across 11 other templates; regenerating it is a separate work unit with its own
  visual-baseline audit.


- [x] T4 — Run final verification and record delivery evidence.
  - Ran the full Rust suite, focused tax/settings/sales/purchases/documents/localization suites, compile, formatting, diff, settings browser, visual baseline, and the complete browser suite.
  - Ran a fresh-database migration smoke check through the application's own embedded migrator on a throwaway `/tmp` database and verified the development database was byte-identical afterward.
  - Evidence: `cargo test` → 1063 passed; `cargo check --all-targets` → 0 errors, 81 warnings; `cargo fmt --check` and `git diff --check` clean; working tree clean; `cargo test tax_` → 98, `settings_` → 23, `sales` → 122, `purchases` → 161, `documents` → 30, `localization_tests` → 39; `bash scripts/e2e.sh tests/test_settings.py` → 6 passed; `bash scripts/e2e.sh tests/test_visual_baseline.py` → 1 passed; `bash scripts/e2e.sh` → 107 passed, 4 intentionally skipped opt-in probes; migrations 36-39 applied cleanly on an empty database with `sale_line_taxes` and `purchase_line_taxes` present and zero failures; `roya.db`, `-shm`, and `-wal` unchanged in size, mtime, and md5. Verdict: PASS. No commit required beyond the T4 evidence commit.

## Acceptance criteria

1. A product price is displayed and stored as net, with a clear tax-inclusive preview derived from its linked taxes.
2. A sale or purchase line snapshots every linked tax's code, name, and rate when the line is created.
3. Multiple taxes are additive; the final line total is rounded to two decimals with half-up rounding.
4. Sale and purchase totals, paid limits, and due amounts use the same tax-inclusive totals.
5. Editing or deactivating a tax later does not change an existing document line's tax snapshot or total.
6. Settings has a permission-gated Taxes tab with create, edit, activate/deactivate, and hard-delete actions.
7. Hard deletion succeeds only when no product association or document snapshot references the tax.
8. Product-linked or document-linked deletion returns an actionable conflict rather than a raw database error.
9. Existing product-tax association behavior and business settings behavior remain operational.
10. Focused tests, the full Rust suite, and applicable browser checks pass, with all skipped or failed checks recorded.

## Applicable checks

- `cargo test tax_`
- `cargo test settings_`
- `cargo test sales`
- `cargo test purchases`
- `cargo test`
- `cargo check --all-targets`
- `bash scripts/e2e.sh tests/test_settings.py` when the browser harness is available
- `git diff --check`
- CSS build when templates add styles requiring generated assets

## Progress and evidence

- Baseline: current branch `main` was clean before branching.
- Current verification: no hard-delete tax route exists; product links are database-restricted; documents have no tax references; totals exclude tax.
- User decision: net price plus the sum of all linked tax rates, with monetary rounding per line.
- Feature document: `odd/tasks/tax-calculation-settings.md`.
- Engram mirror topic: `odd/tax-calculation-settings/tasks`.
- T1 verification: 38 focused tax-snapshot tests and 1005 full Rust tests pass. Independent verification forced and closed three real blockers: non-atomic snapshot/aggregate writes, a legacy public line-write bypass, and unguarded confirmed-line deletion. Final independent verdict: PASS. `cargo check --all-targets` reports 0 errors and 82 warnings; transitional dead-code suppressions are documented for removal in T2.
- T1 delivery: work-unit commit `88176a9` (`feat(tax): persist line tax snapshots atomically`).
- T2 verification: every sale/purchase money path reconciles through the per-line rounded tax-inclusive rule. Independent verification confirmed no `qty * price` bypass, no double counting in refunds or receipt-driven sales, confirmed-document immutability, and correct canonical/localized separation. The required visual baseline was regenerated with 14 intentionally changed captures and no lost appearance; the locale leak guard and duplicate product no-tax notice were fixed. Final verdict: PASS.
- T2 delivery: work-unit commit `a8fa64c` (`feat(tax): make document totals tax-inclusive`).
- T3 verification: the Settings tax catalogue is correctly gated, the business tab is unchanged, hard deletion is server-confirmed and refused for both reference families with history taking priority, and product-association management from the inventory drawer is untouched. Independent verification confirmed the true authorization invariant: this feature makes irreversible administration exclusive to `settings.manage`, while pre-existing `inventory.write` catalogue access to create, rename, re-rate, and activate/deactivate taxes is deliberately unchanged. Final verdict: PASS.
- T3 delivery: work-unit commit `3b9a927` (`feat(tax): add settings taxes tab with hard-delete safeguard`).
- T4 verification: PASS. No failed check and no environment blocker. The four browser skips are intentional opt-in artifact/screenshot probes, not regressions. The development database was verified byte-identical (size, mtime, md5) after every check.
- Delivery accounting: branch `feat/tax-calculation-settings` is 34 files, approximately 7,856 authored changed lines versus `main` (excluding the generated visual baseline). T1 = 2,885, T2 = 2,295, T3 = 2,744. Each cohesive work unit individually exceeds the 400-line advisory budget; the recorded `ask-on-risk` → `stacked-to-main` decision covers the delivery route, and a single-PR route would require maintainer-approved `size:exception`. No push, PR creation, or merge has been performed.
- Follow-ups outside this feature: add `/settings` and `/settings?tab=taxes` to the visual baseline; refine the delete-confirmation panel placement and dialog semantics; rebuild `static/tailwind.css`, which is stale for 14 pre-existing classes across 11 templates. (The fourth follow-up — whether the Products catalogue's `inventory.write` tax access should eventually narrow to `settings.manage` — is **no longer open**: decided and implemented on 2026-09-25 in `odd/tasks/api-tax-permissions.md`, on the web by U1 and in the JSON API by the three `Require<SettingsManage>` gates. See the dated note in Decisions above.)
- Next step: delivery is the user's decision. The feature is implemented, verified, and committed on a local feature branch.

## Delivered

Shipped 2026-09-26 as PR [#107](https://github.com/ematiasm/roya/pull/107), merged as `1d8b444` with `size:exception`.

| Work unit | Implementation | Evidence |
|-----------|----------------|----------|
| T1 | `88176a9` | `9e72cd2` |
| T2 | `a8fa64c` | `20ab076` |
| T3 | `3b9a927` | `a60640d` |
| T4 | `75aa362` | `75aa362` |

This slice was merged without a rebase, so the SHAs cited earlier in this document are the ones on `main` and resolve for anyone. The later slices were rebased onto `main` as each merge advanced it, so their documents carry a `Delivered` section with both identities.
