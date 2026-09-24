# Feature: Initial setup, localization, taxes, and party terms

## Objective

Rebuild the disposable development database from migrations and add a first-run setup wizard that creates the business configuration and initial administrator. Add locale-driven presentation formatting, database-backed business settings, product tax associations, and default debt terms for customers and suppliers.

## Problem

The application currently bootstraps an administrator during startup, has no business configuration or locale context, stores supplier costs with ambiguous date names, lacks product tax relationships, and has no consistent party due-term field. The current database contains disposable test data, so the schema can be corrected before the first supported installation.

## Why

- The first administrator should be configured explicitly instead of generated from environment variables.
- Business language and number/date formats should be selected once and applied consistently to HTML, HTMX, JavaScript, and the API presentation boundary.
- Products need many-to-many tax definitions without storing a single tax id on the product.
- Customers and suppliers need a default credit term, while each sale or purchase keeps its own concrete due date.
- The reset is the right moment to add technical audit timestamps where mutable rows currently record only `updated_by`.

## Scope

### Configuration and setup

- Add `business_settings` with business name, default locale, currency code, timezone, and audit timestamps.
- Add `business_locales` with BCP-47 locale code, language code, display name, enabled state, and audit timestamps.
- Replace automatic startup administrator creation with setup-required detection.
- Add a public `/setup` GET/POST flow with a transaction that creates settings, locales, the administrator, password hash, and protected-role grant.
- Block normal routes while setup is incomplete and make setup unavailable after completion.
- Keep the inactive `sistema` audit sentinel.

- Add an authenticated `/settings` administration panel after setup for business name, default locale, currency, timezone, and enabled locale profiles.
- Add a dedicated settings-management permission, grant it to the protected admin role, and expose the panel in the application sidebar.
- Keep `/setup` one-time only; post-setup changes go through `/settings` and preserve the same validation/audit conventions.

### Locale and formatting

- Resolve one effective request locale from the business default and fallback rules.
- Add centralized number, date, currency, percentage, and input parsing helpers.
- Keep database values and JSON API values canonical; localize presentation and web input only.
- Use locale as the primary formatting key; language selects translations.

### Taxes

- Add `taxes` with code, name, Decimal-as-TEXT rate, active state, and audit metadata.
- Add `product_taxes` with product id, tax id, created actor, and created timestamp.
- Do not add `position`, compound-tax fields, or other calculation metadata yet.
- Keep tax rates on `taxes`; later sale-line snapshots will freeze tax values when tax calculation is implemented.

### Party debt terms

- Rename the existing customer `payment_days` field to `due_days` during the reset.
- Add nullable `due_days` to suppliers.
- Use `NULL` for no default term, `0` for due immediately, and non-negative integers otherwise.
- Customer credit sales default to `sale_date + customer.due_days` when no explicit due date is supplied.
- Supplier credit purchases prefill the confirm dialog with `purchase_date + supplier.due_days`; explicit dates win and Cash purchases remain unaffected.
- Keep concrete due dates on sales and purchases.

### Audit corrections

- Add technical `updated_at` to mutable tables that currently have `updated_by` but no timestamp: transactions, payment_methods, categories, product_supplier_costs, sale_payments, and purchase_payments.
- Rename `current_cost_updated_at` and `previous_cost_updated_at` to `current_cost_date` and `previous_cost_date` because their values are `NaiveDate` business dates.
- Use `purchases.purchase_date` as the effective date when purchase confirmation records supplier costs.
- Stop advancing `users.updated_at` during login; `last_login_at` remains the login timestamp.

## Constraints

- Development database reset is manual and explicit; the application never drops a database at startup.
- Technical artifacts and code comments are English.
- Decimal values remain SQLite `TEXT` and Rust `Decimal`; no formatted values are persisted.
- HTML date inputs and API filters keep canonical ISO values.
- Setup creation is atomic and password hashes never enter logs or plaintext storage.
- Existing protected-role triggers must continue to guarantee an active administrator after setup.
- No push, PR creation, or merge without explicit user request.
- Work-unit commits with focused tests and Conventional Commit messages.

## Manual development reset

1. Stop the Roya server and any process holding the SQLite database.
2. Remove the configured development database file, for example `roya.db`.
3. Remove `roya.db-wal` and `roya.db-shm` when present.
4. Start the application; embedded migrations recreate the schema.
5. Open `/setup` and complete the first-run wizard.

This procedure is never run by the application at startup. Never use it against a non-development or production database.

## Route declaration

- This is substantial ODD work: schema/model/repository changes and presentation flows are delegated to one bounded writer per work unit.
- A separate verifier will be used for the high-risk setup/schema slice when needed.

## TDD mode

- Effective mode: inherit the repository's configured strict TDD when available; runner: `cargo test`.
- The first implementation task must add/identify a test command and must not claim RED/GREEN evidence without observing it.

## Work units

- [x] T1 — Rebuild schema baseline: settings, locales, taxes, product-tax join, due days, audit timestamps, cost-date names, and login timestamp semantics.
      Evidence: `rtk cargo test t1_schema_tests` → 7 passed, 885 filtered out; delegated `rtk cargo test` → 892 passed; parent `rtk git diff --check` passed. No commit created because commit delivery requires explicit user authorization.
- [x] T2 — Add setup-required routing and transactional first-run wizard.
      Evidence: delegated `rtk cargo test setup_tests` → 6 passed, 892 filtered out; parent reran the focused suite → 6 passed; delegated full `rtk cargo test` → 898 passed; independent read-only verification found no concrete T2 defect. No commit created because commit delivery requires explicit user authorization.
- [x] T3 — Add request locale context and centralized display/input formatting.
      Evidence: delegated `rtk cargo test localization_tests` → 13 passed; parent reran focused tests → 13 passed; focused inventory tests → 39 passed; final `rtk cargo test` → 911 passed; `rtk git diff --check` passed. Independent verification found and closed all concrete T3 gaps. No commit created because commit delivery requires explicit user authorization.
- [x] T4 — Wire customer and supplier due-day defaults into forms, APIs, and credit-document flows.
      Evidence: delegated `rtk cargo test due_days_` → 8 passed; parent reran the focused suite → 8 passed; focused customer/supplier/sales/purchases suites passed (94/88/108/150); final `rtk cargo test` → 917 passed; `rtk git diff --check` passed. Independent read-only verification found no concrete T4 defect. No commit created because commit delivery requires explicit user authorization.
- [x] T5 — Add tax catalog and product association management.
      Evidence: delegated `rtk cargo test tax_tests` → 6 passed; parent reran focused tax and inventory suites → 6 and 64 passed; delegated full `rtk cargo test` → 923 passed; `rtk git diff --check` passed. Independent read-only verification found no concrete T5 defect. No commit created because commit delivery requires explicit user authorization.
- [x] T6 — Complete locale presentation coverage across remaining customer, sales, purchase, and full-page HTML surfaces.
      Evidence: strict TDD RED/GREEN across initial detail and follow-up list/supplier/timezone coverage; final `rtk cargo test localization_tests` → 24 passed; full `rtk cargo test` → 934 passed; `rtk git diff --check` passed. Independent final verification found no implementation defect. No commit created because commit delivery requires explicit user authorization.
- [x] T7 — Run focused and full verification, document the manual development reset, and record work-unit evidence.
      Evidence: final `rtk cargo test` → 934 passed; final `rtk cargo test localization_tests` → 24 passed; final `rtk git diff --check` passed; independent final verification found no implementation defect. After the E2E harness was updated for first-run setup and the en-US baseline, `bash scripts/e2e.sh` → 101 passed, 4 opt-in probes skipped, and `git diff --check` passed. Manual reset is documented in this document. No commit created because commit delivery requires explicit user authorization.
- [x] T8 — Add the post-setup `/settings` administration panel and sidebar entry.
      Evidence: strict TDD RED was observed before the settings service, route, permission, migration, and sidebar existed; focused `rtk cargo test settings_` → 6 passed, `ac12_` → 8 passed, `ac21_` → 9 passed, and `setup_tests` → 6 passed; final `rtk cargo test` → 940 passed; focused browser `bash scripts/e2e.sh tests/test_settings.py` → 1 passed; `rtk git diff --check` passed. No commit created because commit delivery requires explicit user authorization.

## Progress and evidence

- Feature document created: `odd/tasks/initial-setup-localization-taxes.md`.
- Initial work-unit commit: pending (parent will create only after explicit user authorization).
- T1 verification: focused schema tests and delegated full suite are green; `git diff --check` is clean.
- T2 verification: `cargo test setup_tests` → 6 passed, 892 filtered out; `cargo test` → 898 passed; `git diff --check` passed. TDD RED was observed before implementation, followed by GREEN and refactor formatting checks.
- T3 verification: `cargo test localization_tests` → 13 passed; `cargo test` → 911 passed; `git diff --check` passed. Independent verification drove fixes for localized web input, product HTML/HTMX, stock fragments, and full-page `html_lang`.
- T4 verification: strict TDD observed 5 passed / 3 failed before the missing supplier and purchase wiring, then `cargo test due_days_` → 8 passed. Focused customer/supplier/sales/purchases suites passed (94/88/108/150); final `cargo test` → 917 passed; `git diff --check` passed. Independent verification found no concrete T4 defect.
- T5 verification: `cargo test tax_tests` → 6 passed; `cargo test inventory_` → 64 passed; final `cargo test` → 923 passed; `git diff --check` passed. Independent verification found no concrete T5 defect.
- T6 verification: strict TDD RED was 4 failed / 923 filtered out before implementation; GREEN `cargo test locale_presentation_` → 4 passed, relevant route suites passed (10/32/68/20/12/19/14/14), `cargo test localization_tests` → 17 passed, full `cargo test` → 927 passed, and `git diff --check` passed. `cargo fmt -- --check` remains blocked by pre-existing formatting differences in unrelated T1–T5 files.
- T7 verification: final `cargo test` → 934 passed; final `cargo test localization_tests` → 24 passed; final `git diff --check` passed. Independent final verification found no implementation defect. The manual reset procedure is documented below. After adapting the E2E harness to the first-run wizard and en-US baseline, `bash scripts/e2e.sh` → 101 passed, 4 opt-in probes skipped; `git diff --check` passed.
- T8 verification: strict TDD covered persistence, validation, default-locale safety, the protected-admin grant, the page gate, and sidebar visibility. Focused settings/auth/nav/setup suites passed (6/8/9/6), the final full Rust suite passed 940 tests, the focused browser settings flow passed, and the final diff check passed.

## Acceptance criteria

1. A fresh development database opens `/setup` and blocks normal application routes.
2. A valid setup submission atomically creates business configuration and an active protected administrator.
3. An invalid or repeated setup submission cannot leave partial configuration or expose setup again.
4. Locale drives number/date/currency presentation while canonical storage and API values remain unchanged.
5. A product can be linked to one or many taxes; duplicate links are rejected.
6. Customers and suppliers expose non-negative nullable `due_days`; concrete sale/purchase due dates remain explicit or correctly defaulted.
7. Purchase-driven supplier cost dates equal the purchase business date.
8. Mutable audit rows have a technical update timestamp when updated.
9. Focused tests and the full Rust suite pass, with failed or skipped checks recorded honestly.

## Next step

T8 is implemented and verified. Run the feature's final read-only verification when requested.
