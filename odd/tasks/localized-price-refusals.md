# Localized price refusals shared by the ladder and the save

## Objective

Give the product price refusals one typed identity and render them in the active locale, so the ladder preview and the product save form always tell the operator the same thing in the same language.

## Problem

The product price rules refuse with raw English strings inside `AppError::Validation(String)`. `src/error.rs` returns that string as the response body, and the global `htmx:responseError` handler in `templates/base.html:129` paints it verbatim into a notice. So a Spanish operator saving a product with a bad price reads `sale_price must be > 0 for products` in a Spanish interface.

The ladder inherited the same strings, which was deliberate at the time: sharing the message is what guarantees the preview cannot disagree with the save. The result is coherent but in English, and the recorded follow-up said so.

Translating only the ladder would make this worse, not better: the ladder would say Spanish and the save form would say English for the identical refusal. Two surfaces disagreeing in language is the exact failure the shared rule exists to prevent.

## Why

- A refusal the operator cannot read is a refusal they cannot act on.
- "The preview never lies about the save" is the ladder's entire value. Coupling the two surfaces through a shared *string* is the weakest possible coupling: it is re-established by hand every time a message is edited, and it breaks silently.
- A typed refusal makes the coupling structural. Both surfaces render the same enum variant through the same function, so they cannot drift in wording, in language, or in existence.

## Decisions

- **Introduce a typed `PriceRefusal` enum** covering every price and cost rule that can change a figure the ladder publishes or the save accepts. `derive_net_sale_price` returns `Result<Decimal, PriceRefusal>` and `validate_effective_prices` returns `Result<(), PriceRefusal>`. No string matching remains on this path.
- **Add one `AppError` variant that carries the code**, keeping the current English text as its rendered body. Every consumer that does not localize — the JSON API, existing tests, anything reading `Display` — behaves byte-identically to today. This is a presentation change, not a behavior change.
- **The renderer is shared, not duplicated.** `price_refusal_key`, `price_refusal_message` and `localized_refusal_error` live in `src/routes/mod.rs` and are called from the ladder, `web_create_product`, `web_edit_product` and `web_apply_line_cost` — **four** surfaces, three originally planned. The purchase record page's "apply line cost" action writes a product's cost through the same service, so it could raise a price refusal; it is localized through the same renderer, not a second one.
- **Scope is the price rules only.** The application's other validation messages stay as they are; localizing all of them is a separate and much larger work unit, not smuggled in here.
- **The refusal set must not change.** The same inputs are accepted and the same inputs refused, with the same conditions, after this task. Only the language changes.
- Technical artifacts, code, comments, tests, and UI copy are English.

## Authorized scope

Repository-local service, error-model, route, localization, test, and feature-document changes. No push, PR creation, or merge without explicit user request. No database reset and no change to the development database. No change to the tax calculation, the price ladder's numbers, or any template structure.

## Route declaration

One bounded delegated-direct ODD work unit: it touches the shared price helpers, the error model, three routes, two catalogs, and the focused tests, which is multiple non-trivial files. One writer. The parent owns task closure, the work-unit commit, verification, and delivery reporting. No SDD phase is created.

## TDD mode

- Effective mode: strict TDD inherited from project configuration (`openspec/config.yaml`, `strict_tdd: true`).
- Test runner: `cargo test`.
- Observe RED before the refactor, then GREEN, then a refactor/check pass.
- RED must be behavioral: a Spanish-locale request must currently answer an English refusal, and the ladder and the save must currently be able to differ.

## Work units

- [x] U1 — Typed price refusals rendered in the active locale by every surface.
  - Added focused tests first and observed RED: a Spanish-locale save answered `{"error":"sale_price must be > 0 for products"}`, and the ladder/save parity test failed on the same request.
  - Introduced `PriceRefusal` (7 variants) in `src/models.rs`. `derive_net_sale_price` returns `Result<Decimal, PriceRefusal>` and `validate_effective_prices` returns `Result<(), PriceRefusal>`; no price rule returns `AppError` any more.
  - Added one `AppError::PriceRefused(PriceRefusal)` whose `Display` and response body are byte-identical to the `Validation` string they replace, so the JSON API and every non-localized consumer are unchanged.
  - One renderer: `price_refusal_key` / `price_refusal_message` / `localized_refusal_error` live in `src/routes/mod.rs` and are called from the ladder, `web_create_product`, `web_edit_product` and `web_apply_line_cost`. A tree-walking test asserts the four mapping needles appear in exactly one file.
  - Added 7 keys to both catalogs; EN rows are byte-identical to the historical English so the API body is preserved, ES rows are real translations.
  - A 17-row characterization test pins the refusal set, including rule ORDER, the Product/Service split and both overflow directions.
  - Evidence: final `cargo test` → 1107 passed; `cargo test product` → 112, `price_ladder` → 36, `product_price_ladder` → 27, `localization_tests` → 39, `purchases` → 162; `cargo check --all-targets` → 0 errors and 81 warnings, byte-identical to the measured HEAD baseline after line-number normalization; `cargo fmt --check` and `git diff --check` clean; `bash scripts/e2e.sh tests/test_products.py` → 24 passed / 1 pre-existing opt-in skip, `tests/test_purchases.py` → 18 passed, `tests/test_visual_baseline.py` → 1 passed. Independent verification found a THIRD unlocalized surface (`web_apply_line_cost`), an htmx-only save test, and two new warnings — one of which was a real defect, imports placed in a test module — all closed before commit. Final independent verdict: PASS on the code; closure corrected the task document and one false sentence in the `AppError::PriceRefused` doc comment. Parent spot check repeated `cargo test product_price_ladder` → 27 passed and `cargo check --all-targets` → 0 errors, 81 warnings. Commit identity is recorded in this document after the work-unit commit.

## Acceptance criteria

1. Every price and cost refusal has a typed identity, not a string.
2. `derive_net_sale_price` and `validate_effective_prices` no longer return `AppError` for a price or cost rule.
3. A product save refused by a price rule answers a message in the active locale, in both enabled locales. Applies to the htmx and the plain-browser branch, which are provably identical because the mapping happens before the `is_htmx` split.
4. The ladder shows that same refusal, in that same locale.
5. The save and the ladder cannot render different text for the same refusal without changing one shared function.
6. The set of accepted and refused inputs is unchanged.
7. The JSON API and every non-localized consumer still receive today's exact English body.
8. No price refusal is matched by string anywhere.
9. A test asserts the ladder and the save agree in both enabled locales.
10. Focused tests, the full Rust suite, and the applicable browser checks pass, with all skips recorded.

## Applicable checks

- `cargo test product`
- `cargo test price_ladder`
- `cargo test product_price_ladder`
- `cargo test localization_tests`
- `cargo test tax_`
- `cargo test`
- `cargo check --all-targets`
- `cargo fmt --check`
- `bash scripts/e2e.sh tests/test_products.py`
- `bash scripts/e2e.sh tests/test_visual_baseline.py`
- `git diff --check`

## Progress and evidence

- Baseline: branch `feat/tax-calculation-settings` at `f6a5304`, working tree clean apart from the unrelated untracked `odd/tasks/pos-counter-sales.md`, which must never be staged by this task.
- Verified: `AppError::Validation` returns its `String` as the body (`src/error.rs:51`) and `templates/base.html:129` renders it verbatim, so a Spanish operator already reads English price refusals from the save form.
- Verified: `web_create_product`, `web_edit_product` and the ladder route all already receive `Extension<LocalizationContext>`, so no new plumbing is needed to localize.
- Feature document: `odd/tasks/localized-price-refusals.md`.
- Engram mirror topic: `odd/localized-price-refusals/tasks`.
- Delivery: work-unit commit `7153f28` (`feat(i18n): localize product price refusals with a typed rule`).
- Next step: delivery is the user's decision. No push, PR creation, or merge has been performed.

### Known gaps, recorded rather than hidden

1. `PriceFieldError::Unreadable` — "invalid sale_price" and friends — deliberately stays an untranslated `Validation(String)`. A Spanish form therefore answers in Spanish for the seven typed price refusals and in English for a mistyped number. That is a declared inconsistency, not an accident: an unparseable field is a parse failure, not a price rule, and folding it into the enum would blur that line.
2. The tree-walking guard proves there is exactly one place that maps a `PriceRefusal` to a `MessageKey`, and exactly one place that carries the English sentences. A hypothetical second renderer returning a hardcoded **Spanish** string would carry neither needle and would pass both guards. No such code exists today; the guard's claim should not be read as broader than that.
3. Both e2e suites run the pinned `en-US` default, so the localized refusals are proven at the HTTP boundary by Rust tests, not by a Spanish browser run. A screenshot-grade Spanish e2e run would need a seeded `es-AR` locale, which is its own work unit.
4. The EN rows carry no final period and the ES rows do. This is forced, not cosmetic drift: the notice templates are `"{action} falló — {message}"` with no terminal punctuation, so the Spanish sentence reads correctly, and unifying would mean adding a period to the API body and breaking the wire contract plus the browser literals.
5. The English sentence has two homes — `PriceRefusal::as_str` (the API body) and the EN catalog row — pinned by a test asserting `en.tr(key) == refusal.as_str()` for every variant. That prevents the two from **diverging**; rewording both together would pass, and would be a deliberate wire change deserving its own test.

### Staging note

`odd/tasks/pos-counter-sales.md` is untracked and belongs to a different task. It must NOT be staged here. Stage the nine modified files and `odd/tasks/localized-price-refusals.md` explicitly.
