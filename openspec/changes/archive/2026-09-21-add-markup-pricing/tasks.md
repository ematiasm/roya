# Tasks: add-markup-pricing (M1 inventory)

> **ARCHIVED 2026-09-21.** This change is delivered and closed; the present-tense truth is now
> `openspec/specs/inventory/spec.md` — the `products` entity list carries `markup_pct`, and the
> Rules section carries the pricing rules: a set markup derives the sale price from the cost and
> ignores the submitted price, the stored price never contradicts its parts (write-time
> recomputation, service-level invariant), the price rule applies to the effective price,
> `markup_pct <= -100` and a `cost_price <= 0` with a markup are rejected, clearing keeps the last
> stored price, the derived value is pinned to cents half-up, a malformed stored markup degrades
> to "no markup", and display never rounds a finer stored value. This folder is history, authored
> directly in its archive home (no move step).

## Review workload

The diff's bulk is mechanical fixture churn: 35 additions of `markup_pct: None` to product struct
literals across 13 files, carrying no behaviour. The behavioural change is confined to two files —
`src/services/inventory.rs` (the derivation, its guards and its tests) and
`src/routes/inventory_api.rs` (the create/update DTOs) — plus the two product templates
(`templates/products.html`, `templates/partials/product_detail.html`). `src/routes/inventory_web.rs`
plumbs the form field through its handlers and holds the web-side tests; the remainder is display
calls (`money_display`) and the ODD feature documents, which live outside `openspec/`.

## Slices

- [x] T1–T3: persistence — migration `20240101000035_add_product_markup.sql` (`markup_pct TEXT
      NULL`, no backfill), the `Product`/`NewProduct`/`UpdateProduct` fields, the repository
      column plumbing and the strict `parse_decimal_opt_strict` reader, fixture churn. Behavior
      untouched: persistence alone computes nothing. (2026-09-21: delivered as `942e328` on
      `feat/product-markup-pricing`; `cargo check --all-targets` 0 errors, `cargo test` 745
      passed, 0 failed.)
- [x] T4–T5: derivation — `sale_price = cost_price * (1 + markup_pct/100)` inside
      `validate_product`, guards (`markup_pct > -100`, `cost_price > 0` with a markup), effective
      price rule, cents pin with half-up rounding; `markup_pct` on the REST DTOs, clearable via
      `double_option`; hardening tests. (2026-09-21: delivered as `1327a10`; `cargo test` 764
      passed, 0 failed.)
- [x] T6–T7: UI — the "Markup %" input in the create modal and the drawer, the `readonly` price
      field with its recalculation hint when a markup is present, and `money_display` in
      `src/models.rs` for the product price render sites. (2026-09-21: delivered as `2df6687`.)
- [x] T8: smoke and browser tests — the first smoke test ever to post `/web/products/edit`
      (save with the stale readonly price; save with a changed markup re-derives) and three
      browser tests over the drawer flow. (2026-09-21: delivered as `d58ec31`; `cargo test` 774
      passed, 0 failed; `scripts/e2e.sh -k products` 16 passed, 1 skipped — the opt-in probe.)
- [x] ODD feature documents: plan and closure of the feature, including the coverage-gap record.
      (Delivered as `7560ee7` and `9d28973`.)

## Coverage gaps found and closed

Verification against the delivered behaviour found six coverage gaps, all closed inside
`1327a10`: derived-to-zero acceptance split by kind (a `Product` refuses a derived `0.00`, a
`Service` accepts it), `m = -99` accepted at the guard's edge, a meaningless `sale_price`
successfully ignored when a markup is set, a manual price proven never to be rounded, and the
rounding midpoint. The last one matters most: the original rounding test rounded a non-midpoint
DOWN, so it would have passed under any strategy and could not prove the half-up choice — the
midpoint test (`10.005 → 10.01`, exact midpoint, half-up away from zero) is the only one that
pins the strategy.

## Verify

- [x] `cargo test` green at close. (Final close: **774 passed / 0 failed**, per `d58ec31`.)
- [x] `cargo check --all-targets` with 0 errors. (0 errors at `942e328` and `1327a10`.)
- [x] `scripts/e2e.sh -k products` green. (16 passed, 1 skipped — the opt-in probe.)
- [x] The repo still performs no `Decimal` division and adds rounding only for the derived price
      (verified by search over `src/`; manual prices pinned exact by test).
- [x] Promotion: `openspec/specs/inventory/spec.md` extended with `markup_pct` and the pricing
      rules; the capability table row in `openspec/specs/README.md` mentions pricing from cost
      and markup; the architecture invariants untouched.
