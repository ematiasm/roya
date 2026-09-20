# Proposal: add-identity-module (M5 Identidad y RBAC)

## Workflow
ODD with OpenSpec artifacts (no SDD phase agents on this machine). Slices of the order of five hundred
lines each, independent verification per slice, chained PRs, archive on merge. The audit half of this
change is sequenced behind the authorization half so the program can stop after Phase A.

## Problem statement
The application has no identity of any kind. `README.md` states it as a feature ("No auth, local
single-user"), and that was true while one person used one machine. The moment the same instance is
reachable from a second device on the shop's network, `tower-http` answers every request for everyone:
whoever opens the browser can price a product, cancel a sale, pay a supplier and rewrite the accounts
ledger. There is no user table, no credential, no session, no middleware, and no way to say "the
warehouse account may move stock but may not see cost prices".

Two consequences follow from the same gap. First, **authorization cannot be retrofitted per handler
without a kernel**: a check scattered through every handler is forgotten exactly where it matters, so the
control has to live in one place that fails closed for routes that nobody remembered to annotate.
Second, **the data records that something happened but never who made it happen**: `sales`, `purchases`,
`stock_movements`, `transactions` and payments all have timestamps and no actor. "Who discounted this?"
and "who cancelled this sale?" are unanswerable today, and they are the first two questions a shop owner
asks when the numbers stop matching.

## Goal
Introduce the identity capability (M5): users with credentials, sessions that can be revoked, roles whose
permission matrix is editable from the interface, a cross-cutting kernel that denies by default and
authorizes per action, and an actor recorded in every mutation the departments write.

## Scope frozen with the user (2026-09-18)
The user chose, of the options presented:

1. **Authentication: login form + session cookie.** Not HTTP Basic, not a trusted identity header. Real
   logout, revocable sessions, a users screen.
2. **Cross-cutting kernel: middleware + extractor.** Departments must not learn that users exist: no
   department reads identity tables, and no department receives the identity service. The kernel depends
   on the identity department; departments depend only on the kernel's extractor types.
3. **Roles and permission assignment editable from the interface.** The admin creates roles and ticks
   permissions per module and action. The permission *catalog* stays seeded in a migration, because a
   permission row only means something if code enforces it; a UI that invents permission rows would tick
   boxes that gate nothing. Catalog codes are compared against the code's own list by a test, so the two
   cannot drift apart silently.
4. **The actor is recorded in every mutation, and shown in the interface.** `created_by` on business
   data, `updated_by` on the mutable master rows, `granted_by` on role assignments, and the actor visible
   in the detail views the departments already have.

## What M5 owns
Tables: `users`, `sessions`, `roles`, `permissions`, `role_permissions`, `user_roles`. Kernel:
`src/security/`. Routes: `identity_web.rs`, `identity_api.rs`. Nothing else in the system may write
these tables; every other module receives only an opaque `Principal`.

## Rules
1. **Deny by default.** Every route requires an authenticated session unless it is in the small public
   allowlist (`/login`, `POST`/`DELETE /api/sessions`, `/static/*`, `/favicon.ico`). A new route is
   protected because it exists, not because someone remembered to annotate it.
2. **Authorization is per action, in the kernel.** Handlers declare the permission they need; the kernel
   answers. An action without a declared permission is a defect, and the enforcement slices close that
   surface module by module.
3. **Username is unique and case-insensitive; display names are not.** Passwords never leave the process:
   only the argon2id hash is stored, and no log line, error message or template may contain a password.
4. **Sessions are revocable and expire.** The cookie carries a random 256-bit token; the database stores
   only its SHA-256 hash, so a leaked database does not hand out live sessions. Validity is decided in
   SQL (`revoked_at IS NULL AND expires_at > now`), revocation is permanent (a trigger refuses to clear
   `revoked_at`), and expiry is absolute with a sliding renewal on activity.
5. **A failed login says nothing.** One generic message for unknown user, wrong password and inactive
   user alike; the verification is constant-time; consecutive failures per username are throttled in
   memory before the password check runs.
6. **The ability to manage roles can never disappear.** The `admin` role is protected (cannot be
   deleted, renamed or re-permissioned) and database triggers refuse to deactivate the last active user
   holding it, or to remove its last holder. The interface explains the refusal; the database is what
   makes it true.
7. **Cross-module SQL stays forbidden.** Identity reaches nothing outside its tables, no department
   queries identity tables, and the audit columns are ordinary integers referencing `users(id)` written
   by the owning module.
8. **Every mutation records its actor.** `created_by` is mandatory on business rows (existing rows are
   backfilled to the bootstrap admin), `updated_by` is set on update, and role grants record who granted
   them and when.
9. **The interface is in Spanish, the artifacts are in English.** Passwords, tokens and hashes never
   appear in templates.
10. **The app keeps working offline.** The new dependencies are build-time only (RustCrypto crates);
    there is still no CDN, no external identity provider and no runtime network call.

## Out of scope for v1
Password reset by email or by token, multi-factor authentication, external providers (OIDC, LDAP),
self-service registration, per-record or per-field row-level sharing, a session manager screen that lists
and revokes other devices, API tokens for machine clients beyond the session cookie, read auditing
(who *looked* at what), rate limiting by source address, and CSRF tokens in every form (the cookie plus
an origin check on unsafe methods is the v1 position, and it is written down in `design.md`).

## Known impact
- **The README's "No Auth" section stops being true** and must be rewritten, together with `env.example`.
- **Every existing route test and the smoke suite** issue unauthenticated requests; a shared authenticated
  test helper is part of slice 1, and its cost is honest: the suite gains a login per test state.
- **Wildcard CORS and cookie authentication contradict each other.** `allow_origin("*")` with credentials
  is rejected by browsers, so the CORS layer stops being a wildcard and becomes same-origin by default,
  configurable through `ROYA_ALLOWED_ORIGINS`. A script using `curl` is unaffected (it sends no Origin).
- **The browser success path moves.** `/` sends an anonymous visitor to `/login`; the browser suite needs
  a login step in its harness, and session expiry during an HTMX request is a new interaction the browser
  suite must cover.
- Audit columns are added to existing tables, which on SQLite means a table rebuild per table with a
  backfill to the bootstrap admin; the slices are ordered per department to keep each diff reviewable.

## Acceptance summary
A fresh database seeds exactly one administrator whose password comes from `ROYA_ADMIN_PASSWORD` or, when
absent, a generated one logged once with a forced change; anonymous access to every non-public route is
refused with a login redirect for HTML and 401 for JSON; a valid login creates a revocable, expiring
session; a handler that declares a permission the principal lacks is refused with 403 and a Spanish
notice; roles can be created and their permission matrix edited without touching code; the seeded admin
role and the last active administrator cannot be removed; every mutation records and displays its actor;
no module queries another module's tables; and the whole thing is covered by the Rust suite plus a browser
slice for the interactions only a browser can see.
