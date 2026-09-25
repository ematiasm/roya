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

- [ ] T3 — Add the Settings Taxes tab and hard-delete safeguard.
  - Add localized tab navigation and reusable tax administration UI.
  - Support create, edit, activate/deactivate, and hard delete under `SettingsManage`.
  - Reject deletion for product associations and document snapshots with actionable conflicts.
  - Preserve existing business settings behavior and inventory product-tax management.
  - Evidence: RED/GREEN settings/tax tests and focused browser coverage, exact commands and commit identity.

- [ ] T4 — Run final verification and record delivery evidence.
  - Run focused suites, the full Rust suite, formatting/diff checks, and applicable E2E checks.
  - Record failures, skips, and environment limitations honestly.
  - Evidence: exact command results and final branch state.

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
- T2 delivery: no commit recorded yet.
- Next step: T3 Settings Taxes tab plus product-association and document-snapshot hard-delete safeguards.
