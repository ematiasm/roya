# Proposal: add-actor-audit (M5, Phase B)

## Status and provenance

This change carries the audit half of the original identity program out of
`openspec/changes/2026-09-18-add-identity-module/` (now archived for Phase A), so the delivered
work and the live plan do not share a folder. It follows the identity module: Phase A — the login
gate, the RBAC core, the users and roles screens, the per-route enforcement, the navigation gating
and the browser slice — is already delivered and is the present-tense truth in
`openspec/specs/identity/spec.md`. Nothing in that spec describes the actor columns of this change.

## Problem statement

The application records that something happened but never who made it happen. `sales`, `purchases`,
`stock_movements`, `transactions` and every payment row carry timestamps and no actor, and
`user_roles` records its grant trail (`granted_by`/`granted_at`, shipped with Phase A) only because
granting a role is itself a privilege change. "Who discounted this?" and "who cancelled this sale?"
are unanswerable today, and they are the first two questions a shop owner asks when the numbers stop
matching. Authorization without an actor explains permissions, not actions.

## Goal

Record the acting principal on every mutation the departments write: `created_by` (mandatory,
FK to `users`, `ON DELETE RESTRICT`) on business tables, `updated_by` (nullable) on the mutable
master rows, the actor visible in the detail views the departments already have — with the value
written by the owning module through the `Principal` the kernel already hands it.

## What this change owns

Audit columns on the existing business tables and their migration; the actor plumbing from
`Principal` into the owning services; the "Registrado por" / "Actualizado por" display in the detail
views. Nothing else in the system writes the columns; identity performs SQL only against identity
tables and the departments write their own audit columns — the cross-module SQL stays forbidden.

## Rules

1. **`created_by` is mandatory on business rows.** Existing rows are backfilled to the bootstrap
   administrator and the column is then enforced `NOT NULL`; a delete of a user referenced by an
   audited row is refused.
2. **`updated_by` is nullable and set on update.** It answers "who touched this last", never "who
   created it".
3. **Lines and join rows inherit the actor of their parent document** (`sale_lines`,
   `purchase_lines`, `product_barcodes`, `role_permissions`): a sale's payment, a purchase's
   payment and a movement produced by a sale carry the acting principal of the originating request,
   so a flow never invents a different actor. The rule is stated in the spec instead of duplicated
   as columns.
4. **The interface shows the display name, never the id**, in the detail views the departments
   already have.
5. **Read auditing stays out of scope**: who *looked* at what is not recorded in v1.

## Out of scope

Password reset by email or by token, multi-factor authentication, external providers (OIDC, LDAP),
self-service registration, per-record or per-field row-level sharing, a session manager screen, API
tokens for machine clients beyond the session cookie, read auditing, rate limiting by source
address.

## Known impact

- Every audited table means a table rebuild per table on SQLite, with a backfill to the bootstrap
  admin; the slices are ordered per department to keep each diff reviewable.
- `openspec/specs/identity/spec.md` gains the audit rules (AC18–AC19) when this change closes; until
  then the identity spec keeps stating that the actor on business tables is not built.
- The dormant ledger items `Role.{created_at, updated_at}` and `Permission.{action, created_at}`
  become production-readable with this change.

## Acceptance summary

Every mutation of an audited table stores the acting user's id and the detail view shows the display
name; a document created inside a flow stores the same actor as the flow's request; existing rows
are backfilled and `created_by` is `NOT NULL` afterwards; a delete of a user referenced by an audited
row is refused; no module queries another module's tables. The whole thing is covered by the Rust
suite in the same style Phase A used: every guard mutation-validated.
