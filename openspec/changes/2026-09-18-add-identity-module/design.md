# Design: add-identity-module

## Architecture
```
                    identity_web.rs / identity_api.rs
                                |
                          IdentityService            (department M5: users, credentials, roles)
                          -> UserRepository, SessionRepository,
                             RoleRepository, PermissionRepository
                                |
        ------------------------------------------------  the kernel boundary
                                |
                        src/security/                     (transversal, not a department)
                        -> password.rs   argon2id hashing and verification
                        -> session.rs    token minting, cookie read/write, TTL policy
                        -> authz.rs      Principal, permission catalog, Require<P>
                        -> guard.rs      middleware (deny by default) + extractors
                                |
   routes/{finance,inventory,sales,purchases,customers,suppliers}/*
                                |
                        Principal (opaque value: user_id, display_name, permission set)
```

Two rules make this safe, and they are the whole point of putting the kernel outside the departments:

1. **The kernel depends on the identity department; departments depend only on the kernel.** No department
   takes `IdentityService` as a dependency, and no department can name a `users` row beyond the integer it
   stores in its own audit column.
2. **No cycles.** Identity knows nothing about sales, stock or accounts; it resolves sessions and
   permissions from its own tables only. Route tests assemble the app exactly like `main` does, so a
   dependency that would create a cycle fails to compile.

The kernel never reaches into a department, and a department never reaches into identity. Enforcement is
declared where the action is: `Require<SalesCreate>` in a handler's argument list.

## Migrations
1. `create_identity_users` — `users` table, the unique case-insensitive username index, the
   `updated_at` trigger convention used elsewhere if one exists, and CHECK constraints on username shape
   and length.
2. `create_identity_sessions` — `sessions` table, unique `token_hash`, indexes on `user_id` and
   `expires_at`, `ON DELETE CASCADE` to `users`, and the trigger that makes revocation permanent
   (refuses to clear `revoked_at`).
3. `create_identity_rbac` — `roles`, `permissions`, `role_permissions`, `user_roles(granted_by,
   granted_at)`, indexes, the two seeded role sets and the seeded permission catalog with a guarded
   `INSERT ... WHERE NOT EXISTS` for every row so re-running cannot duplicate.
4. `create_identity_guards` — the triggers that hold the line the interface cannot be trusted with:
   a protected role cannot be deleted or renamed, its permission rows cannot be deleted; the last active
   user holding a protected role cannot be deactivated; the last grant of a protected role to an active
   user cannot be deleted.
5. Per department, in the audit slices: `add_audit_<department>` — add `created_by`,
   `updated_by` to the department's tables, backfill to the bootstrap admin, then the SQLite table
   rebuild that enforces `NOT NULL` on `created_by` while preserving rows (the pattern already used by
   `add_sales_customer`).

Sessions are pruned opportunistically (expired or long-revoked rows) rather than by a background task:
there is no scheduler in this application, and a session table that only grows is a slow leak.

## Key decisions and tradeoffs
| Option | Chosen | Why / cost |
|---|---|---|
| Session cookie vs HTTP Basic vs trusted header vs bearer token | Session cookie | Real logout and revocable sessions, which a multi-employee shop needs; cost: CSRF surface, cookie flags, and a session lifecycle to test |
| Kernel package vs a department consulted by everyone | Kernel (`src/security/`) | Departments stay ignorant of users and the "no cross-module SQL" invariant survives; cost: one more top-level module, and the kernel boundary has to be enforced by review |
| Deny by default with a public allowlist vs protecting a list of routes | Deny by default | A forgotten annotation fails closed instead of open; cost: the allowlist is a security-critical constant, so it carries a test and a comment per entry |
| Permission catalog seeded in code+migration vs creatable in the UI | Seeded; matrix editable | A permission only exists if code enforces it: a UI that creates permission rows would tick boxes that gate nothing; cost: adding a permission needs a migration, and a test compares catalog and code so they cannot drift |
| `Require<P>` extractor per handler vs router-level guards per module | Extractor per action, with the option of a router guard for whole modules | Action granularity is what the shop asked for ("vendedor que no ve costos"); cost: every handler is touched once, so enforcement is sliced per department |
| Roles as a free-form matrix vs a fixed enum in code | Rows, with `admin` protected | The user wants to define roles without a deploy; the protected role guarantees an administrator always exists; cost: a lockout class of bugs, answered by triggers rather than by UI care |
| Permission set loaded per request vs cached in memory | Per request | Correct and simple: revocation and role edits apply to the next request, and there is no cache to invalidate; cost: one extra query per request, invisible at this scale |
| Store the session token vs its SHA-256 | Hash only | A leaked database does not hand out live sessions, and revocation stays a row update; cost: one extra hash per request and no way to "recover" a session token, which is exactly the point |
| CSRF tokens in every form vs cookie flags plus origin check | `SameSite=Lax`, `HttpOnly`, `Secure` when configured, plus an origin check on unsafe methods | No template churn across ten screens and no new state; cost: honest and written down — a same-site subdomain compromise is not covered, and non-browser clients that send no `Origin` are allowed through (they can forge anything anyway, and they still need a valid session) |
| Login throttling in memory vs a table of attempts | In memory, per username, with an injected clock | Closes the obvious brute force without new tables or cleanup, and stays testable; cost: the counter resets on restart and is per process — written down as a limitation, not a silent gap |
| Test-speed hashing: production params everywhere vs a light hasher in tests | Both: production params by default, light params for tests, plus a test that pins the production parameters | argon2id at OWASP parameters costs ~50-100 ms per login and the suite logs in hundreds of times; cost: the suite does not exercise production cost, so a test asserts the defaults instead |
| Audit columns on every business table vs an event log table | Columns on the owning tables, `sale_lines`/`barcodes`/join rows covered by their parent | "Who created this sale" is answered by reading the sale; cost: a table rebuild per table on SQLite, sliced per department |
| Audit in the same program as authorization vs a separate change | Same change, Phase B, chained | The user asked for it; sequencing it behind Phase A lets the program stop after authorization with a coherent product |
| Wildcard CORS vs same-origin | Same-origin by default, `ROYA_ALLOWED_ORIGINS` to widen | A wildcard with credentials is rejected by browsers, and `curl` is unaffected; cost: an existing browser-based external client would need the variable set — flagged to the user before delivery |
| Password reset by the admin vs by the user | Admin sets a temporary password and the user must change it at next login (`must_change_password`) | No email, no token infrastructure, no lockout when someone forgets; cost: the admin transiently knows the password, which the interface states plainly |

## Session and cookie model
- Cookie `roya_session`, `HttpOnly`, `SameSite=Lax`, `Path=/`, `Max-Age` = absolute TTL; `Secure` added
  when `ROYA_COOKIE_SECURE=1` (documented as required when the app is served over HTTPS).
- Token: 32 random bytes, base64url in the cookie; `sha256` hex in `sessions.token_hash`.
- Absolute TTL `ROYA_SESSION_TTL_HOURS` (default 12) and sliding renewal: when `last_seen_at` is older
  than 30 minutes, a request extends `expires_at` to `now + TTL` in the same statement that updates
  `last_seen_at`. Idle time is therefore also bounded by the TTL, and the renewal is one UPDATE.
- Login always mints a new session (no token reuse, so session fixation has nothing to fix).
- Logout revokes the row and clears the cookie; it is idempotent and never fails on an unknown token.
- Session expiry during an HTMX request answers `401` with `HX-Redirect: /login` and an empty body; a full
  page navigation requests get `303` (for `GET`) or `303` to `/login?next=<path>` with the path validated
  as local before it is used, so the login page cannot be turned into an open redirect. The HTMX branch is
  a browser-suite test, because only a browser can prove htmx 1.9.12 performs that navigation.

## Enforcement wiring
- Middleware resolves the session once per request and inserts `Principal` into request extensions.
- `Require<P: Permission>` is a zero-sized extractor: it reads `Principal` and compares `P::CODE` against
  the resolved set. Missing permission answers `403` with a Spanish notice fragment for HTML/HTMX and a
  JSON body for `/api/*`.
- Whole-module surfaces that are uniformly gated may use a router-level guard instead of a per-handler
  extractor, but only when every route in that group needs exactly the same permission; mixed surfaces
  use per-handler extractors.
- The navigation renders only the entries the principal may read, and pages hide the actions it may not
  perform. The interface never becomes the enforcement: the handler is what refuses.
- `POST /login` and `DELETE /api/sessions` are public by design: logout must work with a dead session.

## Audit
- `created_by` (mandatory, FK to `users`, `ON DELETE RESTRICT`), `updated_by` (nullable, same FK) on
  business tables; pre-existing rows are backfilled to the bootstrap administrator.
- `user_roles` records `granted_by` and `granted_at`: granting a role is itself a privilege change.
- Document lines (`sale_lines`, `purchase_lines`) and join rows inherit the actor of their parent
  document, and the rule is stated in the spec instead of duplicated as columns.
- Detail views show "Registrado por" / "Actualizado por" with the display name, and the interface never
  shows a raw user id.

## Bootstrap
On startup, if no active user holds the protected role, the application seeds `admin`:
- `ROYA_ADMIN_PASSWORD` set → that password, `must_change_password = 0`.
- Absent → a random password generated once, logged through `tracing::warn` as the single place the
  password is ever visible, and `must_change_password = 1`, which confines the session to the password
  change route until it is done.

## Impact on existing code
- `routes/mod.rs`: `AppState` gains `identity_service` and the kernel configuration; `router()` gains the
  middleware; `identity_web.rs` and `identity_api.rs` are merged.
- `main.rs`: reads the new environment variables, logs the bootstrap outcome (never the password unless
  generated), and drops the wildcard CORS origin.
- Every route test and `src/smoke_tests.rs` gains the authenticated helper; unauthenticated requests that
  used to assert 200 now assert the redirect, which is the point of the change.
- `README.md`: the "No Heavy ORM / No Docker / No Auth" section, the module table, the migrations list and
  the environment variables.
- `e2e/tests/test_harness.py`: a login step in the browser harness, and a new slice for the interactions
  (login, forced password change, session expiry mid-HTMX, permission denied on an HTMX form).
- `openspec/specs/`: a new `identity` capability on delivery, plus the `verification` spec's note about the
  new browser cases.

## Verification plan
- Rust suite: login/logout/expiry/revocation, throttling, the generic failure message, permission
  resolution (union of roles, no roles, revoked role), the protected role triggers, the catalog-versus-code
  comparison, and the deny-by-default guard over a representative route list.
- Regression tests are validated the way the repository demands: reintroduce the bug in a throwaway copy
  and confirm the test fails (unauthenticated access reaching a handler; a `Require` that is not enforced;
  a revoked session still resolving).
- Browser suite: login, forced password change, session expiry mid-HTMX, permission-denied fragment on an
  HTMX form, and the navigation reflecting the principal's permissions.
- Manual smoke at the end of each enforcement slice: log in as a restricted user and confirm the refused
  action is refused at the handler, not only hidden in the page.

## Sequencing and review workload
Phase A (authorization) and Phase B (audit). Each slice is its own branch and PR, chained, with `cargo test`
green and an independent verification before the next slice starts.

- **S1** auth foundation: deps, migrations 1-2, models, user/session repositories, `AuthService`
  (bootstrap, login, throttle, logout, session resolution), `security/password.rs`, `security/session.rs`,
  `security/guard.rs` middleware, `identity_web.rs` login/logout, `identity_api.rs` sessions, `AppState`,
  env vars, the authenticated test helper plus every existing test adapted.
- **S2** RBAC core: migration 3 (catalog and seeded roles), `security/authz.rs` (Principal, catalog,
  `Require<P>`), `IdentityService` permission resolution, `role_repo`/`permission_repo`, catalog drift test.
- **S3** users admin: `/users` list, create, deactivate, admin password reset, role assignment with
  `granted_by`, forced password change route, `must_change_password` gate.
- **S4** roles admin: `/roles` list, create, edit, delete, the permission matrix, and the interface
  explanation of every trigger-backed refusal.
- **S5** enforcement: finance and inventory (extractor per action, nav gating, 403 fragment for HTMX).
- **S6** enforcement: sales and customers.
- **S7** enforcement: purchases and suppliers, identity's own screens, dashboard.
- **S8** browser slice for Phase A interactions; README and specs updated; Phase A archived.
- **S9** audit: finance tables + display. **S10** audit: inventory. **S11** audit: sales and customers.
  **S12** audit: purchases and suppliers. **S13** audit: the identity tables themselves (who created a
  role, who granted it). **S14** closing verification and archive of Phase B.

Forecast: Phase A ~2,600 lines across 8 slices; Phase B ~1,900 lines across 6 slices. Every slice is over
the 400-line review budget on its own, so chained PRs are the rule and not a preference.
