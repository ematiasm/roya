# Tasks: add-cost-price-freshness (M1 inventory / M3 purchases)

> **ARCHIVED 2026-09-21.** This change is delivered and closed; the present-tense truth is now
> `openspec/specs/inventory/spec.md` (the products rules carry the stale-cost badge and its
> three gates) and `openspec/specs/purchases/spec.md` (the Interface section carries the
> apply-cost route, and the Authorization section carries the route's `inventory.write` gate
> as a deliberate consequence). This folder is history, authored directly in its archive home
> (no move step).

## Review workload

The branch is thirteen commits (`git log --oneline main..HEAD` from the merge base `40ad8f7`):
the five feature slices below, their interleaved docs commits, and the spec promotion. Its bulk is
tests and documents — the route-test modules in `src/routes/inventory_web.rs` and
`src/routes/purchases_web.rs`, the service-level flag tests in `src/services/purchases.rs`,
the smoke suite, the browser suites (`e2e/tests/test_products.py`,
`e2e/tests/test_purchases.py`), this folder and the ODD feature documents. The behavioural
change proper is confined to small, readable surfaces: one match expression in
`src/routes/inventory_web.rs` (the badge gates), one derived flag in
`src/services/purchases.rs` (the line warning), one short handler plus its route line in
`src/routes/purchases_web.rs` (the action), and two template partials. No migration, no new
column, no stored state — a reviewer should not expect schema work. The final verification is
its own slice (T6's `ca296a6` closes the browser journey, and the close ran `cargo test` over
the whole suite); it is not skipped and is recorded in the Verify section.

## Slices

- [x] T1: the badge — `SupplierService::reference_cost` reused in `product_detail_html`, the
      three gates (reference exists, stored cost non-zero, they differ) as one match
      expression, `StaleCostView` rendered in the supplier-costs card, derived and never
      stored. (2026-09-21: delivered as `c123678` on `feat/cost-price-freshness`; `cargo test`
      782 passed, 0 failed.)
- [x] T2: the badge's browser tests — both values shown when the costs disagree, hidden when
      equal, hidden without supplier rows, hidden at stored zero, preferred-supplier rule.
      (2026-09-21: delivered as `6f84698`; `scripts/e2e.sh -k products` 22 passed, 1 skipped;
      `cargo test` 782 passed, 0 failed, unmoved.)
- [x] T3: the derived line field — `stale_cost` on `PurchaseLineView`, computed from the
      product read the detail view already fetches: strictly higher line cost, non-zero stored
      cost. (2026-09-21: delivered as `d55ed00`; `cargo test` 787 passed, 0 failed.)
- [x] T4: the warning — rendered as a full-width sub-row under the draft line, both numbers,
      badge styling reused, draft-only. (2026-09-21: delivered as `1ac6555`; `cargo test` 790
      passed, 0 failed.)
- [x] T5: the action — `web_apply_line_cost` and its route, gated `Require<InventoryWrite>`,
      draft-only in the handler, no body extractor, the write through
      `InventoryService::update_product` with a `cost_price`-only patch so the markup
      derivation recomputes; route tests for the happy path, the injected body, the
      cross-purchase 404, the permission refusal and the non-draft refusal. (2026-09-21:
      delivered as `844248c`; `cargo test` 797 passed, 0 failed.)
- [x] T6: the browser journey — a stale draft line warns with both numbers, applying updates
      the product (and its markup-derived price), and no stale badge remains afterwards.
      (2026-09-21: delivered as `ca296a6`; `cargo test` 797 passed, 0 failed.)
- [x] ODD feature documents: plan, ledger corrections and closure of the feature (delivered as
      the interleaved `docs(odd)` commits between the slices).

## Verify

- [x] `cargo test` green at close. (Final close: **797 passed / 0 failed**, per `ca296a6`;
      re-verified 2026-09-21 by the documentation-correction pass.)
- [x] The purchase flow still never writes `products.cost_price`: the pre-existing AC10 test
      (`ac10_purchase_never_writes_cost_price_and_satellite_wins`) passes unchanged.
- [x] The wiring-guard gap is recorded, not hidden: the shared fixture's line cost sits below
      the product's stored cost, so the guard never probes the new `hx-post`; the route is
      covered by its own tests and the browser journey.
- [x] Promotion: `openspec/specs/inventory/spec.md` extended with the badge as a derived read
      (existing rules untouched); `openspec/specs/purchases/spec.md` extended with the
      apply-cost route and the authorization consequence (AC10's rule untouched);
      `openspec/specs/README.md` capability rows extended where natural, architecture
      invariants untouched.
