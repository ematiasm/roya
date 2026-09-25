# Settings currency selector

## Objective
Replace manual currency-code entry in the settings panel with a controlled list of supported currencies.

## Problem
`templates/settings.html` uses a free-text `input` for `currency_code`; users can type invalid or inconsistent values and there is no discoverable list. The setup form has the same manual control. The service intentionally accepts any three-letter uppercase canonical code, so existing stored currencies must remain readable and writable.

## Scope
- Add a shared catalog of common business currencies for presentation selection.
- Render a `<select>` in `/settings` and `/setup`, with canonical code values.
- Include the current persisted code as a fallback option if it is not in the catalog.
- Keep the service's existing three-letter uppercase validation and canonical API/DB contract unless a supported catalog restriction is explicitly needed.
- Update E2E to use the select control and add Rust coverage for the rendered options and selected value.

## Constraints
- Currency codes remain canonical uppercase ISO-style values (`ARS`, `USD`, `EUR`, etc.).
- Do not translate or rewrite stored currency values or API response codes.
- Do not add a currency conversion/exchange-rate feature.
- Keep the existing locale/settings fixes and tests intact.

## Tasks

- [x] Add shared currency catalog and option projection.
- [x] Replace setup/settings text inputs with selects.
- [x] Update tests and E2E selectors.
- [x] Run focused and full verification.
- [x] Preserve submitted currency selection on validation redisplay.
- [x] Commit as `feat(settings): add currency selector`.

## Acceptance criteria

- Operators choose a currency from a visible list instead of typing a code.
- The current persisted currency is selected on load, including legacy codes not in the catalog.
- POST form values remain the same `currency_code` field and pass existing validation.
- Settings/setup Rust tests and E2E remain green.

## Verification

- `rtk cargo test setup_tests` — PASS: 7 passed, 960 filtered out.
- `rtk cargo test settings_tests` — PASS: 13 passed, 954 filtered out.
- `rtk cargo test localization_tests` — PASS: 37 passed, 930 filtered out.
- `rtk cargo test` — PASS: 967 passed in 120.76 seconds.
- `bash scripts/e2e.sh` — PASS: 102 passed, 4 skipped in 196.59 seconds.
- `rtk cargo fmt --check` — PASS: no output.
- `rtk git diff --check` — PASS: no output.

## Route record

- Delegated direct implementation from the pushed settings-locale branch; the change spans shared domain presentation, setup/settings templates, and route/E2E tests.
- TDD mode: standard functional checks; no strict TDD mode was requested or detected.

## Progress

- Root cause confirmed: currency is a free-text input and no supported currency catalog exists.
- Added one route-level static catalog and fallback projection shared by setup and settings without changing service validation or persistence.
- Setup and settings now render selected currency selects; focused Rust coverage verifies catalog rendering, persisted selection, and legacy fallback.
- E2E settings now selects EUR through the select control; all other E2E expectations remain unchanged.
- Rollback boundary: remove the currency catalog/projection, page projections, select template changes, and their focused tests/E2E selector update; locale, permissions, API, and DB behavior are untouched.
- Verification follow-up found that validation redisplay projected options from the persisted currency while the rendered field used the submitted currency, so a 400 response could fall back to the first option.
- `page_response` now derives one effective currency value from the submitted form when present, otherwise the persisted settings value, and uses it for both option projection and the rendered field.
- Commit created as `feat(settings): add currency selector`; push is the next delivery step.
