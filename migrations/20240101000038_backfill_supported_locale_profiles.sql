-- Backfill the supported locale profiles for installations created before the
-- first-run setup began seeding every supported profile. The settings form
-- can only offer profiles that exist in business_locales. Fresh installations
-- have no business_settings row yet, so they remain owned by setup and are
-- seeded there instead. Every insert is guarded by locale_code's unique key and
-- preserves the configured default when a legacy database is missing its row.

INSERT INTO business_locales (locale_code, language_code, display_name, is_enabled)
SELECT 'es-AR', 'es', 'Español (Argentina)',
       CASE
           WHEN (SELECT default_locale_code FROM business_settings WHERE id = 1) = 'es-AR' THEN 1
           ELSE 0
       END
WHERE EXISTS (SELECT 1 FROM business_settings)
  AND NOT EXISTS (SELECT 1 FROM business_locales WHERE locale_code = 'es-AR');

INSERT INTO business_locales (locale_code, language_code, display_name, is_enabled)
SELECT 'es-ES', 'es', 'Español (España)',
       CASE
           WHEN (SELECT default_locale_code FROM business_settings WHERE id = 1) = 'es-ES' THEN 1
           ELSE 0
       END
WHERE EXISTS (SELECT 1 FROM business_settings)
  AND NOT EXISTS (SELECT 1 FROM business_locales WHERE locale_code = 'es-ES');

INSERT INTO business_locales (locale_code, language_code, display_name, is_enabled)
SELECT 'en-US', 'en', 'English (United States)',
       CASE
           WHEN (SELECT default_locale_code FROM business_settings WHERE id = 1) = 'en-US' THEN 1
           ELSE 0
       END
WHERE EXISTS (SELECT 1 FROM business_settings)
  AND NOT EXISTS (SELECT 1 FROM business_locales WHERE locale_code = 'en-US');
