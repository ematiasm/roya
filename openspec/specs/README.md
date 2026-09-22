# Roya — consolidated specifications

This directory is the **canonical, present-tense description of what the system does today**.
Each capability has its own folder. When a change is delivered, its spec is promoted here and
the change folder moves to `changes/archive/`; the specs below always describe the current state,
never a proposal.

## Capabilities

| Capability | Module | What it covers |
|---|---|---|
| [finance](finance/spec.md) | M0 | Accounts, transactions, derived balance, payment methods and their per-account allowlist, document references |
| [inventory](inventory/spec.md) | M1 | Categories, products and services, barcodes, stock movements, derived stock and reorder suggestion, pricing derived from cost and markup, the stale-cost badge over the supplier reference cost |
| [sales](sales/spec.md) | M2 | Sales, lines, payments, document numbering, cash and credit, cancellation |
| [purchases](purchases/spec.md) | M3 | Suppliers, per-supplier cost history, purchases, purchase orders from the reorder suggestion, cancellation, and the stale-cost warning with its apply-cost action on a draft line |
| [customers](customers/spec.md) | M4 | Customers, the protected walk-in, credit rules, derived receivables and ageing, receipts that group a handover across sales |
| [documents](documents/spec.md) | — | The cross-department documents index: one read-only feed over sales, sale payments, purchases, purchase payments, stock movements and customer receipts, newest first, with type/user/date filters and a text search; it opens for any one of the four read tiers and narrows its content to the tier that opened it, while owning no table and no write path |
| [identity](identity/spec.md) | M5 | Users, revocable sessions, roles with editable permission matrices, the permission catalog, the deny-by-default kernel, the route → permission contract, the role-grant trail, and the actor audit: every mutation of the audited tables records the authenticated principal, with pre-audit rows attributed to the `sistema` sentinel and the interface showing names, never ids |
| [verification](verification/spec.md) | — | The two test layers, the boundary between them, the browser harness contract, and the rule that makes a green suite mean something |

## Architecture invariants

These hold across every capability and are the rules a new module must respect.

1. **One database, hard logical boundaries.** All modules share `roya.db`. A module writes only
   its own tables and reaches other modules exclusively through their services. No cross-module SQL.
   Identity is a peer that is not a department: it is a transversal kernel (invariant 12), not one
   more module.
2. **Upstream points at downstream, never the reverse.** Sales and purchases may reference accounts,
   products and payment methods. Finance and inventory must never reference sales or purchases.
   Concretely: a foreign key from a payment to the transaction it produced is allowed; the reverse
   is not, and finance receives a document number only as an opaque `reference` string.
3. **Derived state is never stored as truth.** Account balance, product stock, sale total, sale debt,
   customer balance, receivable ageing, reorder suggestion and the total of a receipt are all computed.
   A cached column may exist for convenience but is never used to answer a read. Where a stored value
   could contradict its own parts, it must not be stored: a receipt's stored total was removed for exactly
   that reason after a partial failure was shown to leave it claiming more than it had applied.
4. **Money and quantities are `Decimal`, stored as `TEXT` in SQLite.** `NUMERIC` affinity would cast
   to `REAL` and silently lose digits, so sums are performed in Rust.
5. **Every money movement caused by a document is traceable in both directions.** The movement knows
   its source document number (`reference`); the payment knows the movement it produced
   (`transaction_id`) and the movement that reversed it (`refund_transaction_id`).
6. **Artifacts are written in English** (database columns, code, identifiers, commit messages,
   specs). The user interface is in Spanish.
7. **The application works offline.** CSS and JavaScript are served from `static/`; there is no CDN
   or runtime network dependency.
8. **Document numbering is universal.** `doc_sequences(doc_type, year, last_number)` issues
   `YYYY-TYPE-NNNNNN` numbers on confirmation, never on draft. New document types are a row, not a
   schema change.
9. **What code cannot be trusted with belongs in the database.** Cross-row invariants are enforced with
   CHECK constraints, partial unique indexes and triggers, not only in the service layer: the walk-in is
   permanent, a payment cannot be grouped under another customer's receipt. Where a trigger is used,
   remember that SQLite does not fire `BEFORE DELETE` triggers for rows removed by REPLACE conflict
   resolution unless `PRAGMA recursive_triggers` is enabled on the connection, which this project does.
   The guarantee covers accidental and programmatic writes; it does not cover an actor deliberately
   dropping the triggers or altering the schema.
10. **Multi-step operations across modules are not atomic.** The project deliberately does not share a
    transaction between modules. Every expected rejection is validated before any write, and the residual
    is detected rather than hidden: a movement whose `reference` looks like a document number must be
    claimed by a payment as its `transaction_id` or `refund_transaction_id`.
11. **Verification has two layers and a boundary.** The Rust suite covers the server; the browser suite
    covers interaction, focus, navigation, dialogs and the URL. A behaviour only a browser can see belongs
    in the browser suite and may not be approximated by a Rust test that checks an attribute. And a
    regression test is validated by reintroducing the bug and watching it fail: three times here a test
    that looked like a guard was decoration.
12. **Authorization is declared at the route, answered by the kernel.** A department handler declares
    the permission its action needs (`Require<P>`); the security kernel resolves the principal and
    answers. Deny by default: an undeclared route still needs a valid session, and a route that needs
    a permission declares it — the route → permission table in the identity spec is the contract.
    No department reads identity tables or takes the identity service as a dependency; the kernel is
    transversal and identity is not a department. The actor audit extends the same rule to actions:
    the actor a mutation records is the principal the kernel resolved for that request, never
    anything the request's payload can supply, and the owning module writes its own audit columns.
    The declaration has an any-of form for a screen several tiers each open a part of
    (`RequireAny<S>` over a set of catalog codes): the route admits any ONE of them and the
    page then narrows its content to the tier that opened it, so the screen is an index of
    what the principal may already read and never a new grant. The documents index is the
    worked example, and an any-of gate over an empty set denies.

## Verification

The suite is the evidence for everything stated below. Two layers matter:

- **Unit and route tests** cover rules, guards and error mapping.
- **`src/smoke_tests.rs`** drives the real router end to end (extractors, templates, HTMX fragments)
  and holds a generic wiring guard that fails when a rendered form target does not resolve, when the
  HTTP verb is not routed, when a concrete id is hardcoded on a typed-id page, or when a URL is built
  dynamically. Its rules exist because each one caught a real bug or a real blind spot.

A green suite is not evidence by itself. Regression tests are validated by reintroducing the bug in a
throwaway copy and confirming they fail.
