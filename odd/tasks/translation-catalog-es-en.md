# Translation catalog: Spanish and English

## Objective
Add a presentation-only translation catalog for `es` and `en`, while preserving regional locale formatting (`es-AR`, `es-ES`, `en-US`) and canonical API/database values.

## Problem
The application currently localizes numbers, dates, currency, and timezone through `LocalizationContext`, but user-facing text is hard-coded and mixed between Spanish and English. `language_code` is resolved but unused.

## Scope
- Static English and Spanish translation catalogs.
- Closed translation keys with English fallback for unsupported or missing language codes.
- Server-rendered templates and HTMX fragments use the business configuration language.
- First vertical coverage: base shell, sidebar, notices, setup, login, password, forbidden page, and settings.
- Subsequent coverage: dashboard/accounts, products, customers, then remaining sales/purchases/suppliers/documents/users/roles.
- No translated persistence values, API enum values, ISO dates, currency codes, or locale codes.
- No new i18n dependency and no user-language negotiation in this change.

## Constraints
- Technical artifacts and catalog keys are English; Spanish values use neutral/professional Spanish.
- `locale_code` remains the HTML `lang` value; `language_code` selects the catalog.
- Service/API error text must not be blindly translated; only presentation surfaces are changed.
- Tests must preserve canonical form values and API contracts.
- Use work-unit commits: tests and behavior stay together.

## T1 — Catalog kernel and application shell

- [x] Add a closed `MessageKey` catalog with complete `en` and `es` entries.
- [x] Add `tr`, interpolation, and count helpers to `LocalizationContext`.
- [x] Fall back to English for unsupported/missing language codes.
- [x] Translate base shell, sidebar, generic notices, accessibility labels, and page `<html lang>` remains regional.
- [x] Add catalog parity, fallback, interpolation, and shell rendering tests.
- [x] Commit: `feat(i18n): add Spanish and English catalog`

T1 verification:
- `rtk cargo test localization_tests`: 28 passed, 916 filtered out.
- `rtk cargo test`: 944 passed.
- Targeted `rustfmt --edition 2021 --check` on all four T1-modified Rust files: passed.
- `rtk cargo fmt --check`: repository-wide check remains red because of unrelated pre-existing formatting drift outside the T1 files.
- `rtk git diff --check`: passed.
- Independent verifier found no T1 behavioral blocker; T2+ surfaces remain intentionally pending.

## T2 — Setup and access surfaces

- [x] Translate setup form labels, validation/help text, and locale display copy.
- [x] Translate login, password, forbidden, and post-setup settings surfaces.
- [x] Preserve setup bootstrap behavior: pre-setup uses the existing fallback until configuration is saved.
- [x] Keep POST values, permissions, redirects, and CSRF behavior unchanged.
- [x] Add Spanish/English rendering and settings round-trip tests.
- [x] Commit: `feat(i18n): translate setup and access surfaces`

T2 verification:
- Focused bilingual access test: 1 passed.
- `rtk cargo test setup_tests`: 6 passed.
- `rtk cargo test settings_tests`: 7 passed.
- `rtk cargo test routes::identity_web::tests`: 20 passed.
- `rtk cargo test`: 946 passed.
- Targeted rustfmt checks on T2 core implementation/test files: passed.
- `rtk cargo fmt --check`: repository-wide check remains red from unrelated pre-existing formatting drift.
- `rtk git diff --check`: passed.
- The cumulative diff also contains T1 inventory/purchase notice changes; these are preserved T1 work, not T2 scope leakage.
- Commit remains pending because the user did not explicitly request commits.

## T3 — Core CRUD presentation

- [x] Translate dashboard, account, product, and customer visible labels and notices.
- [x] Translate HTMX partials and route-built HTML while preserving canonical option values.
- [x] Add locale-aware route/partial tests and picker message transport.
- [x] Commit: `feat(i18n): translate core CRUD surfaces`

T3 verification:
- Focused T3 tests: 4 passed.
- `rtk cargo test routes::web::tests`: 14 passed.
- `rtk cargo test routes::inventory_web::tests`: 39 passed.
- `rtk cargo test routes::customers_web::tests`: 10 passed.
- `rtk cargo test localization_tests`: 33 passed.
- `rtk cargo test`: 950 passed.
- Targeted rustfmt checks on all T3-modified Rust files: passed.
- `rtk cargo fmt --check`: repository-wide check remains red from unrelated pre-existing drift.
- `rtk git diff --check`: passed.
- Independent verifier found no T3 behavioral blockers; picker transport and canonical values are covered. Browser E2E remains pending for T4.
- Commit remains pending because the user did not explicitly request commits.

## T4 — Remaining presentation coverage

- [x] Translate sales, purchases, suppliers, documents, users, and roles templates/partials.
- [x] Translate remaining route-built notices and static UI messages without changing API responses.
- [x] Update E2E expectations to be language-aware and run the full applicable suite.
- [x] Document intentionally untranslated domain/API strings and technical exceptions.
- [x] Commit: `feat(i18n): complete Spanish and English presentation catalog`

T4 verification and fix results:
- The concrete T4 findings are fixed: the purchase picker smoke assertion counts the exact `data-picker="product"` mount marker; HTMX action labels use closed catalog entries; purchase inactive status, cancellation-reason placeholders, and operational examples use presentation catalog entries.
- Final acceptance also localizes display-only seeded RBAC and payment-method copy while preserving canonical persistence and transport values. The 24th seeded permission, `settings.manage`, now has closed bilingual display text in both the Spanish and English role matrices.
- `rtk cargo test`: 959 passed.
- `bash scripts/e2e.sh`: 102 passed, 4 skipped (203.03s).
- `rtk cargo test localization_tests`: 36 passed, 923 filtered out.
- `rtk cargo test routes::users_web::tests`: 15 passed, 944 filtered out.
- `rtk cargo test roles_web::tests`: 15 passed, 944 filtered out.
- `rtk cargo test routes::sales_web::tests`: 33 passed, 926 filtered out.
- `rtk cargo test routes::purchases_web::tests`: 69 passed, 890 filtered out.
- `rtk cargo test routes::customers_web::tests`: 11 passed, 948 filtered out.
- `rtk cargo test routes::suppliers_web::tests`: 20 passed, 939 filtered out.
- Targeted `rustfmt --edition 2021 --check` on all 22 currently modified Rust files: passed.
- `rtk git diff --check`: passed.
- `rtk cargo fmt --check`: exit 1 from unrelated pre-existing drift in unmodified files (including `src/error.rs`, `src/main.rs`, repository, API, and service files); all 22 modified Rust files pass the targeted check.
- No commit existed when this verification was recorded; the complete T1-T4 catalog is now authorized for one local feature commit. Push and PR remain out of scope.

## Display-only seeded catalogs

The migrations remain the canonical persistence source. Role codes, permission codes, seeded role names/descriptions, permission descriptions, and payment-method names continue to be stored and submitted exactly as seeded; REST and form contracts do not change.

Presentation uses closed code-based mappings:

- the 24 seeded permission descriptions;
- the names and descriptions of the four seeded roles (`admin`, `vendedor`, `cajero`, `deposito`);
- the seeded payment methods `Cash` and `Transfer`, plus the synthetic `unassigned` account label.

Known seeded codes receive English or Spanish display text. Custom role, permission, and payment-method values fall back to the persisted name/description. Option values, method IDs, POST values, role codes, and permission codes remain canonical.

## Intentional untranslated values and exceptions

- API JSON, database values, form values, enum/status values, ISO dates, currency codes, locale codes, IDs, URLs, HTML `name` attributes, HTMX verbs/targets, usernames, and role/permission codes remain canonical and untranslated. The translated `data-action` value is presentation-only and continues to feed the existing generic HTMX notice behavior.
- The `data-picker` attribute is a technical island mount marker; its value is now the stable `product`, while `data-picker-*` attributes remain transport/configuration keys. The smoke assertion counts only `data-picker="product"`, not every attribute beginning with `data-picker`.
- Stored product, customer, supplier, user, role, and payment-method data is never rewritten for display. Only the closed seeded mappings above replace known seed copy at render time.

| Intentional example | Why it remains untranslated |
|---|---|
| `Distribuidora Sur…` | Neutral example supplier/business name; proper operational example data, not canonical seed data. |
| `ticket-123` | Neutral example receipt identifier; the label around it is translated while this example remains stable. |
| `555-1234`, `11 5555-5555`, and the `555-01xx` E2E phone samples | Neutral example phone numbers in their plausible formats; translating digits or punctuation would invent data. |
| `SKU` | Neutral technical/domain abbreviation used as a field label. |
| `YERBA-500` | Example SKU/identifier; translation would change the operational example data. |
| `Yerba 500g` | Example product/brand text; it represents stored product data rather than interface copy. |
| `Ana Pérez…`, `María Pérez…` | Neutral proper-name examples used for customer/display-name fields. |
| `supervisor…` | Machine-code example constrained to a technical role-code shape. |
| `Supervisor…` | Neutral proper-name/display-name example paired with the code example. |
| `caja1…` | Neutral operational username example retained in English; Spanish uses the localized `usuario1…` guidance because it is not a stored value. |
| `settings.manage` | Canonical permission code displayed in the role matrix; only its seeded description is mapped to bilingual display text. |

## Acceptance criteria

- Both `es` and `en` catalogs have the same closed key set.
- `es-AR` and `es-ES` render Spanish; `en-US` renders English.
- Unsupported language codes fall back to English without leaking keys.
- `<html lang>` remains the regional locale code.
- Canonical API/database values remain unchanged.
- Relevant Rust and E2E checks pass; failures are recorded honestly.

## Verification

- `rtk cargo test`
- `bash scripts/e2e.sh` when browser dependencies are available
- `rtk git diff --check`
- Catalog parity and translation regression tests

## Route record

- Route: delegated direct implementation, not SDD.
- Mapping trigger: the localization surface spans more than four files; a narrow `explore` task mapped it first.
- Writer trigger: each translation slice touches multiple non-trivial files; implementation is delegated to one writer per work unit.
- TDD mode: standard functional checks; no strict TDD mode was requested or detected for this continuation.
- Review delivery: follow ordinary repository policy; no push or PR was requested for this continuation.

## Progress

- 2026-09-24: Created feature document on `feat/translation-catalog-es-en` from clean `origin/main`.
- T1 catalog kernel, application shell, and verification: complete; included in the local feature commit.
- T2 setup/access surfaces, verification, and cumulative tests: complete; included in the local feature commit.
- T3 core CRUD presentation surfaces, verification, and cumulative tests: complete; included in the local feature commit.
- T4 remaining presentation coverage, seeded RBAC/payment display acceptance fixes, bilingual route assertions, and verification: complete; included in the local feature commit.
- Final `settings.manage` bilingual catalog fix, direct role-matrix assertions, ledger correction, and verification: complete; included in the local feature commit.
- The complete T1-T4 catalog is authorized for one local feature commit; push and PR remain out of scope.
