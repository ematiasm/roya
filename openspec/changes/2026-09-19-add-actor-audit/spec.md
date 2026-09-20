# Spec: add-actor-audit (M5, Phase B)

This is the planned state, not the current one: the identity capability spec
(`openspec/specs/identity/spec.md`) states what ships today and deliberately does not describe
these rules. The design's "Audit" section is the source.

## Audit columns (added to existing tables)

- `created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT` and `updated_by INTEGER NULL`
  (same FK) on `accounts`, `transactions`, `payment_methods`, `categories`, `products`,
  `stock_movements`, `sales`, `sale_payments`, `customer_receipts`, `customers`, `suppliers`,
  `product_supplier_costs`, `purchases`, `purchase_payments`, `roles`, `permissions`, and
  `created_by`/`updated_by` on `users` themselves (self-referencing, nullable for the bootstrap
  administrator).
- Existing rows are backfilled to the bootstrap administrator; `created_by` is then enforced
  `NOT NULL`.
- Lines and join rows (`sale_lines`, `purchase_lines`, `product_barcodes`, `role_permissions`)
  inherit the actor of their parent document and get no columns of their own.

## Rules

- **Every insert into an audited table writes `created_by = principal.user_id`;** every update sets
  `updated_by`. The column is written by the owning module from the `Principal` the kernel resolves
  per request; no department reads identity tables or takes the identity service as a dependency.
- **A document created inside a flow carries the acting principal of the originating request**: a
  sale's payment, a purchase's payment, a movement produced by a sale — a flow never invents a
  different actor.
- **The interface shows the display name, never the id**, in the detail views the departments
  already have ("Registrado por" / "Actualizado por").
- A delete of a user referenced by an audited row is refused (`ON DELETE RESTRICT`), and the
  interface names the refusal in Spanish.

## Interface

- Detail views of the audited documents gain the actor rows; no new screen, no new route.
- The users list already exists; no surface change beyond the display names.

## Acceptance criteria

- [ ] AC18: every mutation of an audited table stores the acting user's id, and the detail view
      shows the display name; a document created inside a flow stores the same actor as the flow's
      request.
- [ ] AC19: existing rows are backfilled and `created_by` is `NOT NULL` afterwards; a delete of a
      user referenced by an audited row is refused.

## Verification

- The per-table migration and backfill: copy the real database, run the chain, confirm every
  pre-existing row's actor and the NOT NULL enforcement.
- Route tests per department: the actor is asserted on create, update, and inside a flow (sale
  confirm writing its own movement and payments), each mutation-validated in the style Phase A used.
- The boundary grep: no department queries identity tables; the audit columns are ordinary integers
  written by the owning module.
