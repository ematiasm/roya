# Product price ladder and tax catalogue de-duplication

## Objective

Remove the duplicated tax catalogue from the Products screen so tax definitions live only in Settings, and replace the scattered product price facts with one readable, server-computed price ladder: cost → markup → net sale price → taxes → tax-inclusive price.

## Problem

Two defects came out of the tax calculation feature.

1. `templates/products.html:68-96` still renders the full tax catalogue — create, edit, deactivate — after Settings gained a dedicated Taxes tab. The same form is now reachable from two screens with two different permissions, which is exactly the ownership ambiguity the previous feature had to document around.
2. A product's price facts are scattered across the drawer: net price in the header line 31, tax-inclusive price in lines 35-41, the sale-price field with its "recalculated" note in lines 107-120, and the tax breakdown card around line 188. There is no ordered price ladder, and the derived figures only move after the product is saved.

## Why

- Two screens editing the same catalogue is not a permission model, it is an accident. Tax definitions are configuration; product-tax association is inventory. One owner each.
- A price that cannot be read as a ladder cannot be sanity-checked by a human. Cost, markup, net and gross must be visible together or the operator cannot tell whether a price is sane before selling it.
- The price ladder must never be computed twice. Money rounding duplicated in browser JavaScript is how a drawer and a document end up one cent apart.

## Decisions

- **The net sale price is the truth.** `products.sale_price` stays net and canonical. Taxes never modify it.
- **The ladder is computed on the server only.** The browser never applies the markup formula or the tax rounding. A `change` event triggers an htmx refresh of the ladder fragment and the server returns the already-formatted, already-localized ladder.
- **The stored net price still re-derives server-side on save** from cost and markup; that behaviour is unchanged and stays the enforcement point.
- **Linking or unlinking a tax refreshes the whole drawer**, so the ladder updates from server truth with no extra round trip.
- **The Products screen keeps product-tax association only.** Creating, editing, activating, deactivating and deleting a tax definition belong to Settings alone **on the web surface**.
- **"Settings alone" is a claim about the WEB surface, and only that.** After U1 the web has one address space for a tax definition, `/web/settings/taxes…` behind `settings.manage`. It is not a claim about the whole application, and it must never be written as one.
- **OPEN DECISION, not settled by this task: the JSON API residual.** `POST /api/taxes` (create), `PUT /api/taxes/{id}` (rename, re-rate, and `is_active`) and `POST /api/taxes/{id}/deactivate` in `src/routes/inventory_api.rs` are all still `Require<InventoryWrite>`. So an `inventory.write` principal — a JSON client, not a browser — can still define a tax. This is **pre-existing and is not a U1 regression**: U1 removed the second *web* catalogue, not the API, and narrowing API permissions is a separate product decision the user has not made. U1 therefore leaves those handlers and their tests untouched and records the residual instead of quietly asserting an exclusivity it does not have. Closing it means changing `/api/taxes` permissions to `settings.manage`, which is out of U1's authorized scope and needs its own decision.
- **A previously pinned expectation is deliberately INVERTED.** T3 of the tax feature left a test on purpose: `tax_inventory_catalogue_keeps_its_own_write_access_for_an_inventory_writer` asserted that a principal holding only `inventory.write` still created, renamed, re-rated and activated/deactivated taxes from the Products catalogue. T3's own authorization note conceded this was a loose boundary it had deliberately not narrowed. U1 narrows it, so that test is **replaced, not deleted** — by `tax_web_definition_administration_is_exclusive_to_settings_manage`, whose name is scoped to the web and whose body probes only `/web/taxes…`. It asserts the same claim with the sign flipped: the inventory web routes are gone (404, not 403) and `settings.manage` still owns every web definition mutation. A reviewer comparing against T3's evidence should read this as a change of web-surface policy, not a regression.
- **What is deliberately NOT inverted:** the product-tax ASSOCIATION stays on `inventory.write` (`POST /web/product-taxes`, `POST /web/product-taxes/unlink`, and `POST`/`DELETE /api/products/{id}/taxes…`). Linking a tax to a product is inventory state; only its DEFINITION is configuration.
- Technical artifacts, code, comments, tests, and UI copy are English.

## Scope

### Tax catalogue de-duplication

- Remove the tax catalogue section from `templates/products.html`.
- Keep the product drawer's linked-tax list and link/unlink association UI.
- Remove web routes and partials that become unreachable for catalogue administration, and their tests. The JSON API is unchanged — including its permissions, which this unit does not touch.
- Update the authorization note so it is scoped to the web surface: after this change **no `inventory.write` web route** can create, rename, re-rate, or activate/deactivate a tax, while `settings.manage` still can. State the JSON API residual as an open decision instead of implying global exclusivity.

### Price ladder

- Add a single ordered pricing card in the product drawer: cost, markup percentage, net sale price, each tax with its rate and amount, total taxes, and tax-inclusive price.
- The card is a server-rendered fragment refreshed on change of cost, markup, or sale price, and on every tax link/unlink.
- The net price stays canonical and is never rewritten by a tax change.
- Manual-price products (no markup) keep their manual net price and the ladder still shows taxes and the tax-inclusive price.
- Products whose derived price rounds to zero keep the existing rejection behavior.
- The ladder is readable in every enabled locale, using the existing currency and percentage helpers.

## Authorized scope

Repository-local template, route, service, static asset, localization, test, and feature-document changes. No push, PR creation, or merge without explicit user request. No database reset and no change to the development database.

## Route declaration

Substantial delegated-direct ODD work. Two work units, each delegated to one bounded writer because each touches multiple non-trivial files. The parent owns scope, task closure, work-unit commits, verification, and delivery reporting. No SDD phase is created.

## TDD mode

- Effective mode: strict TDD inherited from project configuration (`openspec/config.yaml`, `strict_tdd: true`).
- Test runner: `cargo test`.
- Each behavior work unit must observe RED before implementation, then GREEN, then a refactor/check pass.
- Browser checks use the existing Playwright harness; the visual baseline is regenerated only for captures this work actually changes, and the diff is audited.

## Work units

- [x] U1 — Remove the duplicated tax catalogue from the Products screen.
  - Delete the catalogue section from `templates/products.html` and any now-unreachable partials, web routes, and tests.
  - Keep product-tax association working from the drawer.
  - Prove no `inventory.write` **web** route can create, rename, re-rate, or activate/deactivate a tax, while `settings.manage` still can. The JSON API keeps its `inventory.write` tax-definition routes; that residual is recorded as an open decision, not closed here.
  - Regenerate and audit the affected `products` visual capture.
  - Evidence: RED/GREEN focused tests, exact commands, commit identity.

- [ ] U2 — Add the server-computed product price ladder.
  - Add the ordered ladder fragment and its refresh endpoint.
  - Cover cost, markup, net, per-tax amounts, total taxes, and tax-inclusive price.
  - Cover manual-price products, taxes added/removed, and locale formatting.
  - Prove the browser never computes money: no markup formula or rounding is duplicated in static JavaScript.
  - Evidence: RED/GREEN focused tests, exact commands, commit identity.

- [ ] U3 — Run final verification and record delivery evidence.
  - Run focused suites, the full Rust suite, compile, formatting, diff, and the applicable browser checks.
  - Record failures, skips, and environment limitations honestly.
  - Evidence: exact command results and final branch state.

## Acceptance criteria

1. The Products screen no longer offers tax create/edit/deactivate; the same form is not reachable from two screens.
2. Linking and unlinking a tax from a product still works, and refreshes the drawer.
3. No `inventory.write` **web** route can create, rename, re-rate, or activate/deactivate a tax after U1. Scoped to `/web/…` on purpose: the JSON API's `POST /api/taxes`, `PUT /api/taxes/{id}` and `POST /api/taxes/{id}/deactivate` remain `Require<InventoryWrite>` and are out of this criterion's scope, left unchanged by U1 and recorded above as an open decision.
4. The product drawer shows one ordered ladder: cost, markup, net sale price, per-tax amount, total taxes, tax-inclusive price.
5. The ladder is computed on the server; no money formula or rounding rule is duplicated in browser JavaScript.
6. Changing cost or markup refreshes the ladder immediately, and saving still re-derives the stored net price.
7. Adding or removing a tax updates the tax-inclusive price without changing the stored net price.
8. A manual-price product keeps its manual net price and still shows taxes and the tax-inclusive price.
9. The ladder renders correctly in every enabled locale.
10. Focused tests, the full Rust suite, and applicable browser checks pass, with all skips and failures recorded.

## Applicable checks

- `cargo test product`
- `cargo test tax_`
- `cargo test settings_`
- `cargo test`
- `cargo check --all-targets`
- `cargo fmt --check`
- `bash scripts/e2e.sh tests/test_products.py`
- `bash scripts/e2e.sh tests/test_visual_baseline.py`
- `bash scripts/e2e.sh tests/test_settings.py`
- `git diff --check`
- CSS build only if a template introduces a utility class missing from committed `static/tailwind.css`

## Progress and evidence

- Baseline: branch `feat/tax-calculation-settings` at `75aa362`, working tree clean.
- Verified defect: `templates/products.html:68-96` duplicates the Settings tax catalogue.
- Verified defect: price facts are split across `templates/partials/product_detail.html` lines 31, 35-41, 107-120, and 188.
- User decision: the net sale price is the truth; taxes derive the gross and never rewrite the net.
- Feature document: `odd/tasks/product-price-ladder.md`.
- Engram mirror topic: `odd/product-price-ladder/tasks`.
- U1 delivery: work-unit commit `121293d` (`refactor(tax): make Settings the only web surface for tax definitions`).
- Next step: U2 server-computed product price ladder.

### U1 — tax catalogue de-duplication

- **RED** (`cargo test tax_`, before any implementation change): 2 failed / 97 passed.
  - `tax_products_screen_offers_no_tax_catalogue` — `the Products page still serves the tax catalogue (id="tax-catalog-section")`.
  - `tax_web_definition_administration_is_exclusive_to_settings_manage` (named this way at the prose-correction round below) — `no inventory.write route may administer a tax definition (POST /web/taxes): <div class="flex flex-col gap-2">…` with `left: 200, right: 404`, i.e. the request had *created* a tax. The same run also surfaced a defect in the new test itself: the re-rate through `/web/settings/taxes/edit` deactivated the tax, because that route reads `is_active` off the form's active box exactly as the removed Products row form did. Corrected to carry `is_active=1`, matching the real row form.
- **GREEN** (`cargo test tax_`): 99 passed, 0 failed. Full suite `cargo test`: 1064 passed, 0 failed. `cargo check --all-targets`: 0 errors, 81 warnings (unchanged from the pre-change baseline of 81; the two the removal would have introduced — the now-unused `NewTax`/`UpdateTax` imports and `checkbox_is_checked` — were deleted in the refactor pass). `cargo fmt --check` clean, `git diff --check` clean.
- **Reachability, proved before deleting anything.** Every removed symbol had exactly one caller, all inside the catalogue:
  | Removed | Why it is orphaned |
  |---|---|
  | `templates/partials/tax_list.html` | its only `{% include %}` was `products.html:74`; the file is deleted, so nothing includes it |
  | `GET /web/taxes` (`web_tax_list`) | its only `hx-get` was the catalogue Refresh button; answers into `#tax-list`, which no longer exists |
  | `POST /web/taxes` (`web_create_tax`) | its only `hx-post` was the catalogue create form; the only `inventory.write` tax-definition create **on the web** (the JSON API's is a separate surface, untouched) |
  | `POST /web/taxes/edit` (`web_edit_tax`) | its only `hx-post` was the catalogue row form; the only `inventory.write` rename/re-rate/activate **on the web** |
  | `POST /web/taxes/deactivate` (`web_deactivate_tax`) | its only `hx-post` was the catalogue row's Deactivate button |
  | `TaxListPartial`, `tax_list_html`, `tax_mutation_response`, `TaxForm` | only reachable from those five; `TaxForm` had no reference outside the three deleted handlers |
  | `ProductsTemplate.taxes` + `products_page`'s `list_taxes()` | fed only the deleted section; the page no longer reads a tax definition at all |
  | the `taxes-changed` `HX-Trigger` | no template ever listened for it; already dead, removed with `tax_mutation_response` |
  **Deliberately untouched:** `/api/taxes`, `/api/taxes/{id}` and their sub-routes (JSON API, shared with the e2e fixtures), `TaxService`, `TaxRepository`, `/web/product-taxes`, `/web/product-taxes/unlink`, `product_tax_mutation_response` (the drawer refresh), `/web/settings/taxes…`.
- **Localization: no key became unreferenced, so the EN and ES catalogs are unchanged.** All twelve keys the deleted markup used are still referenced elsewhere — `ProductTaxes` by the drawer's linked-tax card and breakdown header, and `ProductCreateTax`, `ProductSaveTax`, `ProductNoTaxesConfigured`, `ProductCode`, `ProductRate` all by `templates/partials/settings_taxes.html`; the `Common*` keys are shared across nine or more templates. Verified by re-scanning every template and Rust source after the removal.
- **Authorization notes corrected**, since both stated the old policy as current fact: the URL-decision comment in `src/routes/settings_web.rs` and the canonical-surface comment in `src/settings_tests.rs`. The historical task document `odd/tasks/tax-calculation-settings.md` is left as the record of what T3 decided at the time; the new decision lives here.
- **Visual baseline — regenerated, then audited by changed-set containment.** The harness failed first (`products: element gone from body/div[0]/div[1]/main[1]/div[3]/div[2]`, and the same for the other four affected captures) before regeneration, which is the honest order. After `ROYA_VISUAL_BASELINE=write`: **77 capture names before, 77 after, 0 added, 0 removed, 72 byte-identical, exactly 5 changed** — `products`, `products:hover`, `products-drawer`, `products-create-under-filter`, `products-create-under-filter:hover`. Per changed capture: **only elements disappeared** (17, 2, 17, 17 and 2), **0 appeared, and 0 surviving elements changed any recorded style**. The 17 removed paths are exactly the deleted subtree: the `div[2]` section wrapper, its header (`h2[0]` "Taxes" + `button[1]` Refresh), the `#tax-list` island, and `form[2]` (create form) with its three labelled inputs and `button[3]`. Because the removed block was the *last* child of its grid, no sibling index shifted, which is why no unrelated capture moved. `bash scripts/e2e.sh tests/test_visual_baseline.py` then passes in verify mode (1 passed).
- **Note on the 5 captures, stated rather than glossed:** the brief named "the `products` page capture", and one name is not the whole truth. Three capture names render the `/products` body — the page, the page with the drawer open, and the create-under-filter state — so all three families legitimately change. No capture outside `/products` moved.
- **Dev database untouched:** `roya.db` mtime stayed at `2026-09-25 16:15:05`, before this session began, and the harness's own assertion that the server never opened it passed on every e2e run.
- **CSS:** `scripts/build-css.sh` deliberately NOT run. This change only deletes markup and introduces no new class, and the visual baseline — which compares computed styles of every element on every screen — passes unchanged for all 72 unaffected captures, so the committed stylesheet needs nothing. Reported rather than run on the off chance.
- **Pre-existing harness limitation found, not fixed (out of U1 scope):** `products-drawer:hover` and `products-drawer:hover-skipped` are absent from the baseline entirely, because the drawer loop in `test_visual_baseline.py` calls `_fingerprint` directly instead of `capture()` and so never records a hover state. Unrelated to this change; flagging for U3.
- **No commit.** Uncommitted on `feat/tax-calculation-settings` at `75aa362`, per the bounded-writer brief.

### U1 correction round — the exclusivity claim was scoped too wide

Independent verification found a real prose defect in U1, and it is worth stating plainly because U1's own handoff claimed the opposite.

- **What was wrong.** U1's new notes asserted as GLOBAL fact that tax-definition administration is exclusive to `settings.manage` and that no `inventory.write` route can create, rename, re-rate or activate/deactivate a tax. That is true **only of the `/web/…` surface**. `POST /api/taxes` (`create_tax`, `src/routes/inventory_api.rs:398`), `PUT /api/taxes/{id}` (`update_tax:419`) and `POST /api/taxes/{id}/deactivate` (`deactivate_tax:442`) are all still `Require<InventoryWrite>`, so an `inventory.write` principal acting as a JSON client can still define a tax. The false sentences were not only wrong, they were load-bearing: the test docstring claimed "nothing reaches `create_tax`, `update_tax` or `deactivate_tax` through the inventory gate" while naming the very handlers that still do.
- **Not a U1 regression.** Those API permissions are pre-existing (they predate this branch's work and are pinned by `tax_api_preserves_inventory_permission_boundaries`). U1 removed a second *web* catalogue, not the API. The defect is in the prose, not the code — which is exactly the failure mode a comment-only change can cause and a test cannot catch.
- **What was corrected, and what was deliberately NOT.** The claim is now scoped to the web in all seven places that made it: the URL decision in `src/routes/settings_web.rs`, the canonical-surface note and the refusal-test docstring in `src/settings_tests.rs`, the module header and the association note in `src/routes/inventory_web.rs`, the U1 section header and test docstring in `src/tax_tests.rs`, and Decisions / Scope / Work units / Acceptance Criterion 3 here. Each now names the residual explicitly instead of implying exclusivity. The JSON API handlers, their permissions and their tests are **untouched** — narrowing `/api/taxes` to `settings.manage` is a separate product decision nobody has made, and it is recorded as an open decision above rather than performed.
- **The test was renamed, not weakened.** `tax_definition_administration_is_exclusive_to_settings_manage` → `tax_web_definition_administration_is_exclusive_to_settings_manage`, so the name alone cannot be read as a global claim, and the body still probes only `/web/taxes…`. It remains non-vacuous: all six web addresses must 404, the stored tax must be byte-for-byte unchanged, and `settings.manage` must still create, rename, re-rate, deactivate, activate and hit the hard-delete safeguard. A re-added web route fails it exactly as before.
- **Non-vacuity of the renamed test was proven by mutation, not asserted.** `POST /web/taxes` was temporarily re-registered as a stub returning 200; the test failed with `left: 200, right: 404`, and the stub was reverted immediately. `grep '"/web/taxes' src/routes/*.rs` returns nothing afterwards, so the route table is clean.
- **Verification:** this round changed comments, docstrings, one test name and prose only — **no markup, no route, no template, no stylesheet**. The visual baseline was therefore NOT regenerated, and no e2e run was needed. `cargo test tax_`, `cargo test product`, `cargo test settings_`, `cargo test`, `cargo check --all-targets`, `cargo fmt --check` and `git diff --check` all re-run clean.
- **Independent final verdict: PASS.** Parent spot check repeated `cargo test tax_` → 99 passed.
