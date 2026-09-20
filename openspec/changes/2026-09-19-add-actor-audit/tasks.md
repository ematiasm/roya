# Tasks: add-actor-audit (M5, Phase B)

## Provenance

Carried out of `openspec/changes/2026-09-18-add-identity-module/` when Phase A closed, so the
archived folder records only what was delivered. The slices below are T26–T31 of the original
tasks list, unchanged in intent. Phase A is delivered and is the present-tense truth in
`openspec/specs/identity/spec.md`; this change extends that capability with the audit rules and
then archives its own scope.

## Phase B — audit

- [ ] T26 (S9): audit migration for finance tables (`accounts`, `transactions`, `payment_methods`),
      actor plumbing from `Principal`, "Registrado por"/"Actualizado por" display, tests for
      AC18–AC19 on the finance surface.
- [ ] T27 (S10): audit for inventory tables (`categories`, `products`, `product_barcodes` inherits,
      `stock_movements`) — the movement's actor is the request's principal even when the movement is
      produced by a sale or purchase confirm.
- [ ] T28 (S11): audit for sales and customer receipts (`sales`, `sale_lines` inherits, `sale_payments`,
      `customer_receipts`) — the sale's payment rows carry the same actor as the flow's request.
- [ ] T29 (S12): audit for purchases, suppliers and supplier costs (`purchases`, `purchase_lines`
      inherits, `purchase_payments`, `suppliers`, `product_supplier_costs`).
- [ ] T30 (S13): audit for the identity tables themselves (`users`, `roles`, `permissions`) and the
      grant trail display (`user_roles.granted_by/granted_at` already shipped; surface it readably).
- [ ] T31 (S14): closing verification — `cargo test` green with the audit tests mutation-validated,
      `openspec/specs/identity/spec.md` extended with the audit rules and AC18–AC19, and this
      change's folder archived.

## Verify

- [ ] `cargo test` green at the end of every slice, with every guard mutation-validated
      (reintroduce the bug, watch the test fail).
- [ ] AC25 holds for every new comparison against a database-written timestamp.
- [ ] `cargo check --all-targets` with no new errors and no new `#[allow(dead_code)]` /
      `#[allow(unused_imports)]` attributes.
- [ ] Independent verification of each slice before its PR.

## Archiving

- [ ] On merge of Phase B: extend `openspec/specs/identity/spec.md` with the audit rules and
      AC18–AC19, and move this change's folder to `openspec/changes/archive/`.
