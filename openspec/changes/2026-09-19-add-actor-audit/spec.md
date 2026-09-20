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
- Every row that exists when a department's audit migration runs predates the audit, and no user
  may exist yet (migrations run before the application's bootstrap creates the administrator), so
  the migration creates its OWN actor: an inactive, roleless account named `sistema` whose stored
  hash is deliberately malformed (the verifier treats an unparseable hash as a failed verification,
  a behaviour pinned by a test in `security/password.rs`), and attributes every pre-existing row
  to it. The migration is therefore independent of the bootstrap: it runs before it, in either
  order, and on a fresh install the bootstrap still creates the administrator through its ordinary
  creation path, so exactly one active administrator exists afterwards, holding the protected role.
  The sentinel is inserted only when there is something to attribute; the interface shows it as the
  display name "Sistema (anterior al registro)" in the users list, where it explains the
  attribution instead of hiding it.
  **Assumption, written down because it is one:** rows that predate the audit were not created by
  any person the system knew, so a person's name on them would be an invented attribution — this
  is the honest-attribution rule of Phase B. The rejected alternative was backfilling to the
  bootstrap administrator: it would (a) attribute system-seeded and historical rows to a person who
  did not create them, (b) turn the bootstrap's recovery path into the only one a fresh install
  ever runs, leaving the creation path dead in production, and (c) couple the migration to the
  bootstrap having run. A synthetic generic "system" account with a real-credential-shaped hash
  was considered for the same reasons and rejected for the same reasons; the malformed-hash
  sentinel keeps the account unusable as a login, which is the property that matters.
- Lines and join rows (`sale_lines`, `purchase_lines`, `product_barcodes`, `role_permissions`)
  inherit the actor of their parent document and get no columns of their own.

## Rules

- **Every insert into an audited table writes `created_by = principal.user_id`;** every update sets
  `updated_by`. The actor travels explicitly as an argument — route (the `Principal` the kernel
  resolves per request) → service → repository — with no global and no request-local: the owning
  module passes the acting user's id down, and a mutation produced by another document carries the
  SAME actor as the originating request. No department reads identity tables or takes the identity
  service as a dependency; the display names the views render are resolved in the wiring layer
  (`routes/mod.rs`), which the AC20 boundary scan explicitly permits to touch identity.
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
