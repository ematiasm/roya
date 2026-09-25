# Fix settings locale and language selection

## Objective
Allow a fresh installation to change its default locale and presentation language from `/settings`.

## Problem
The setup flow currently persists only the selected locale profile. The settings page can edit existing profiles but cannot offer locales that were never inserted, so a new installation configured as `es-AR` cannot switch to `en-US` or `es-ES`. `language_code` is intentionally derived from the selected locale code; the UI does not need a separate mutable language field.

## Scope
- Seed every supported locale profile during first-run setup: `es-AR`, `es-ES`, and `en-US`.
- Backfill missing supported locale profiles for existing installations with a data migration, disabled unless they are the existing default.
- Enable only the selected setup locale.
- Keep language codes canonical and derived from the locale profile (`es`/`en`).
- Keep the settings update contract, validation, permissions, and persistence atomic.
- Add regression coverage proving a fresh setup and an existing installation expose all locales and can switch the default locale to English, then render English presentation.

## Tasks

- [x] Seed all supported locale profiles in setup.
- [x] Backfill missing supported profiles for existing installations.
- [x] Update setup/migration assertions and add settings switch regression.
- [x] Run focused and full verification.
- [x] Commit as `fix(settings): allow changing locale and language`.

## Acceptance criteria

- A fresh setup with `es-AR` stores `es-AR`, `es-ES`, and `en-US` profiles, with only `es-AR` enabled.
- An existing installation with only one profile is backfilled with the missing supported profiles, disabled and without changing its current default.
- `/settings` lists all three profiles and accepts changing the default to `en-US` with English enabled.
- After the change, the next request renders English presentation.
- Existing setup, settings, localization, migration, and E2E tests remain green.

## Verification

- `rtk cargo test setup_tests`
- `rtk cargo test settings_tests`
- `rtk cargo test localization_tests`
- `rtk cargo test`
- `bash scripts/e2e.sh`
- `rtk cargo fmt --check`
- `rtk git diff --check`

## Route record

- Delegated direct implementation: the bug spans setup persistence, settings rendering, and route integration tests.
- TDD mode: standard functional checks; no strict TDD mode was requested or detected.

## Progress

- Created `fix/settings-locale-language` from current `origin/main`.
- Root cause confirmed: first-run setup persisted only one locale profile.
- Added migration `20240101000038_backfill_supported_locale_profiles.sql`, which is guarded by existing settings and profile rows, inserts all supported profiles disabled unless one is the configured default, and leaves fresh setup ownership unchanged.
- Existing single-profile upgrade coverage starts at migration `20240101000037`, backfills the three canonical profiles, and switches to `en-US` through `/settings`; fresh-install coverage remains green.
- Settings presentation now moves the configured default locale to the first form position while preserving the relative order of every other profile. `SettingsForm::to_update` restores the submitted complete profile set to repository order before service validation and persistence, so the positional form contract remains intact.
- Verification evidence (all requested checks passed):
  - `rtk cargo test settings_tests` — 10 passed, 953 filtered out.
  - `rtk cargo test setup_tests` — 6 passed, 957 filtered out.
  - `rtk cargo test localization_tests` — 37 passed, 926 filtered out.
  - `rtk cargo test` — 963 passed.
  - `bash scripts/e2e.sh` — 102 passed, 4 skipped in 201.14s.
  - `rtk cargo fmt --check` — passed.
  - `rtk git diff --check` — passed.
- Commit created as `fix(settings): allow changing locale and language`; push is the next delivery step.
