# Tasks: add-actor-audit (M5, Phase B)

## Provenance

Carried out of `openspec/changes/2026-09-18-add-identity-module/` when Phase A closed, so the
archived folder records only what was delivered. The slices below are T26–T31 of the original
tasks list, unchanged in intent. Phase A is delivered and is the present-tense truth in
`openspec/specs/identity/spec.md`; this change extends that capability with the audit rules and
then archives its own scope.

## Phase B — audit

- [x] T26 (S9): audit migration for finance tables (`accounts`, `transactions`, `payment_methods`),
      actor plumbing from `Principal`, "Registrado por"/"Actualizado por" display, tests for
      AC18–AC19 on the finance surface. (2026-09-20: delivered on `feat/audit-finance`; the
      cross-department call sites of `TransactionService` entered this slice because
      `created_by NOT NULL` on `transactions` is the inter-department plumbing itself —
      `SalesService`/`PurchasesService` confirm/cancel/pay flows and the collection flow pass the
      request's actor down. The pre-existing rows' actor is the migration-created `sistema`
      sentinel, per the rule above; see the slice's section in `odd/tasks/identity-rbac.md` for
      the demonstrated upgrade sequence, the mutation table and the verification numbers.)
- [x] T27 (S10): audit for inventory tables (`categories`, `products`, `product_barcodes` inherits,
      `stock_movements`) — the movement's actor is the request's principal even when the movement is
      produced by a sale or purchase confirm. (2026-09-20: delivered on `feat/audit-inventory`;
      migration 31 reuses migration 30's `sistema` sentinel — its guarded insert is defensive only —
      and the sale/purchase confirm/cancel flows extend the same `actor` argument they already thread
      for their finance rows into the inventory `record_movement` call. See the slice's section in
      `odd/tasks/identity-rbac.md` for the demonstrated upgrade sequence, the mutation table and the
      verification numbers.)
- [x] T28 (S11): audit for sales and customer receipts (`sales`, `sale_lines` inherits, `sale_payments`,
      `customer_receipts`) — the sale's payment rows carry the same actor as the flow's request.
      (2026-09-20: delivered on `feat/audit-sales-customers`; migration 32 reuses the sentinel and its
      guarded insert is defensive only, `sale_lines` inherits the sale's actor, and the `customers`
      table — which the original task list never assigned — lands here with the department that
      owns it. See the slice's section in `odd/tasks/identity-rbac.md` for the demonstrated upgrade
      sequence, the constraint-preservation audit, the mutation table and the verification numbers.)
- [x] T29 (S12): audit for purchases, suppliers and supplier costs (`purchases`, `purchase_lines`
      inherits, `purchase_payments`, `suppliers`, `product_supplier_costs`). (2026-09-20: delivered
      on `feat/audit-purchases-suppliers`; migration 33 reuses the sentinel and its guarded insert
      is defensive only, `purchase_lines` inherits the purchase's actor, and a line change stamps
      the draft's `updated_by` — the document was edited. See the slice's section in
      `odd/tasks/identity-rbac.md` for the demonstrated upgrade sequence, the
      constraint-preservation audit, the mutation table and the verification numbers.)
- [x] T30 (S13): audit for the identity tables themselves (`users`, `roles`, `permissions`) and the
      grant trail display (`user_roles.granted_by/granted_at` already shipped; surface it readably).
      (Delivered on `feat/audit-identity-tables`, migration 20240101000034. With it, every table the
      original audit list named is audited: the twelve business tables of T26–T29 plus
      `users`/`roles`/`permissions` here. `role_permissions` carries no columns of its own — the
      join-row rule: a matrix row inherits its role's actor, and the matrix edit stamps the role's
      `updated_by` — and `user_roles` keeps its own `granted_by`/`granted_at` trail, which the users
      screen finally renders ("«rol»: otorgado por «nombre» el «fecha»"). `users` is ALTERed, not
      rebuilt (nullable self-referencing audit columns: NULL = the system, rendered as "el
      sistema"); `roles`/`permissions` are rebuilt with the seven guard triggers dropped and
      recreated byte-identically. See the slice's section in `odd/tasks/identity-rbac.md` for the
      demonstrated upgrade sequence, the constraint-preservation audit, the mutation table and the
      verification numbers.)
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
