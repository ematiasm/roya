# Roya — consolidated specifications

This directory is the **canonical, present-tense description of what the system does today**.
Each capability has its own folder. When a change is delivered, its spec is promoted here and
the change folder moves to `changes/archive/`; the specs below always describe the current state,
never a proposal.

## Capabilities

| Capability | Module | What it covers |
|---|---|---|
| [finance](finance/spec.md) | M0 | Accounts, transactions, derived balance, payment methods and their per-account allowlist, document references |
| [inventory](inventory/spec.md) | M1 | Categories, products and services, barcodes, stock movements, derived stock and reorder suggestion |
| [sales](sales/spec.md) | M2 | Sales, lines, payments, document numbering, cash and credit, cancellation |
| [purchases](purchases/spec.md) | M3 | Suppliers, per-supplier cost history, purchases, purchase orders from the reorder suggestion, cancellation |

## Architecture invariants

These hold across every capability and are the rules a new module must respect.

1. **One database, hard logical boundaries.** All modules share `roya.db`. A module writes only
   its own tables and reaches other modules exclusively through their services. No cross-module SQL.
2. **Upstream points at downstream, never the reverse.** Sales and purchases may reference accounts,
   products and payment methods. Finance and inventory must never reference sales or purchases.
   Concretely: a foreign key from a payment to the transaction it produced is allowed; the reverse
   is not, and finance receives a document number only as an opaque `reference` string.
3. **Derived state is never stored as truth.** Account balance, product stock, sale total, sale debt,
   customer balance and reorder suggestion are all computed. A cached column may exist for
   convenience but is never used to answer a read.
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

## Verification

The suite is the evidence for everything stated below. Two layers matter:

- **Unit and route tests** cover rules, guards and error mapping.
- **`src/smoke_tests.rs`** drives the real router end to end (extractors, templates, HTMX fragments)
  and holds a generic wiring guard that fails when a rendered form target does not resolve, when the
  HTTP verb is not routed, when a concrete id is hardcoded on a typed-id page, or when a URL is built
  dynamically. Its rules exist because each one caught a real bug or a real blind spot.

A green suite is not evidence by itself. Regression tests are validated by reintroducing the bug in a
throwaway copy and confirming they fail.
