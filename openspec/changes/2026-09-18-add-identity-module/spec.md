# Spec: add-identity-module

## Entities

### users
- `id PK`, `username TEXT NOT NULL` (trimmed, 3-64, `^[a-z0-9]([a-z0-9._-]*[a-z0-9])?$`, unique
  case-insensitively through a `COLLATE NOCASE` unique index)
- `display_name TEXT NOT NULL` (≤ 128)
- `password_hash TEXT NOT NULL` (argon2id PHC string; never returned by any read path)
- `is_active INTEGER NOT NULL DEFAULT 1`
- `must_change_password INTEGER NOT NULL DEFAULT 0`
- `last_login_at TEXT NULL`, `created_at`, `updated_at`
- Rules: a username is unique and immutable in practice (rename is allowed but audited); users are never
  deleted, they are deactivated; an inactive user cannot log in and cannot hold a session.

### sessions
- `id PK`, `token_hash TEXT NOT NULL UNIQUE` (the sha256 digest of the cookie token, base64url-encoded;
  the token itself is never stored)
- `user_id NOT NULL REFERENCES users(id) ON DELETE CASCADE`
- `created_at`, `expires_at`, `last_seen_at` NOT NULL, `revoked_at TEXT NULL`, `user_agent TEXT NULL` (≤ 256)
- Indexes on `user_id`, `expires_at`.
- Trigger: `revoked_at` cannot go from set to NULL (revocation is permanent).
- Rule: a session is valid iff `revoked_at IS NULL AND expires_at > :now` — decided in SQL, not only in Rust.

### roles
- `id PK`, `code TEXT NOT NULL UNIQUE` (machine name, `^[a-z][a-z0-9_]*$`), `name TEXT NOT NULL` (Spanish
  label shown in the interface), `description TEXT NULL` (≤ 256)
- `is_system INTEGER NOT NULL DEFAULT 0` — a protected role
- `created_at`, `updated_at`
- Seed: `admin` (`is_system = 1`, all permissions), and three ordinary roles `vendedor`, `cajero`,
  `deposito` with their matrices, every insert guarded so re-running cannot duplicate.
- Rules: a protected role cannot be deleted, its `code` cannot change, and its permission rows cannot be
  removed. A role assigned to any user cannot be deleted (FK RESTRICT); the interface says which users
  block it.

### permissions
- `id PK`, `code TEXT NOT NULL UNIQUE` (`<module>.<action>` or `<module>.<resource>.<action>`),
  `module TEXT NOT NULL`, `action TEXT NOT NULL`, `description TEXT NOT NULL`, `created_at`
- Seeded catalog, 23 rows: `dashboard.read`; `finance.read`, `finance.write`,
  `finance.methods.manage`; `inventory.read`, `inventory.write`, `inventory.stock.write`;
  `sales.read`, `sales.create`, `sales.cancel`; `customers.read`, `customers.write`,
  `customers.collect`; `purchases.read`, `purchases.create`, `purchases.cancel`,
  `purchases.costs.read`, `purchases.costs.write`; `suppliers.read`, `suppliers.write`;
  `identity.users.read`, `identity.users.manage`, `identity.roles.manage`.
- Rules: the catalog is data seeded by migration; the application does not create permission rows at
  runtime. A test asserts the seeded catalog equals the catalog compiled into `security/authz.rs`, so a
  code-level permission cannot exist without its row and a seeded row cannot exist without an enforcer.

### role_permissions
- `role_id NOT NULL REFERENCES roles(id) ON DELETE CASCADE`, `permission_id NOT NULL REFERENCES
  permissions(id) ON DELETE CASCADE`, primary key `(role_id, permission_id)`.

### user_roles
- `user_id NOT NULL REFERENCES users(id) ON DELETE CASCADE`, `role_id NOT NULL REFERENCES roles(id) ON
  DELETE RESTRICT`, `granted_by NOT NULL REFERENCES users(id) ON DELETE RESTRICT`, `granted_at NOT NULL`,
  unique `(user_id, role_id)`.

### Audit columns (added to existing tables)
- `created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT` and `updated_by INTEGER NULL` on
  `accounts`, `transactions`, `payment_methods`, `categories`, `products`, `stock_movements`, `sales`,
  `sale_payments`, `customer_receipts`, `customers`, `suppliers`, `product_supplier_costs`, `purchases`,
  `purchase_payments`, `roles`, `permissions`, and `created_by`/`updated_by` on `users` themselves
  (self-referencing, nullable for the bootstrap administrator).
- Existing rows are backfilled to the bootstrap administrator; `created_by` is then enforced `NOT NULL`.
- Lines and join rows (`sale_lines`, `purchase_lines`, `product_barcodes`, `role_permissions`) inherit the
  actor of their parent document and get no columns of their own.

## Derived reads
- `effective_permissions(user) = UNION of permissions of the user's roles`, resolved per request; no cache.
- `principal(user) = { user_id, username, display_name, must_change_password, effective_permissions }`.
- `is_protected_role(role) = roles.is_system = 1`.
- `active_admin_count = COUNT(users JOIN user_roles JOIN roles WHERE roles.code = 'admin' AND
  users.is_active = 1)` — the quantity the triggers protect.
- Permission matrix for the interface: the 23 seeded permissions grouped by `module`, with the roles that
  hold each one.

## Rules
- **Authentication.** `POST /login` verifies the password against the stored argon2id hash in constant
  time. Unknown username, wrong password and inactive user all answer the same message ("Usuario o
  contraseña incorrectos") with the same status, and a cost-equivalent argon2 hash of a dummy credential is
  computed even when the username does not exist, so the answer time does not leak existence. On success: a new session row, the
  `Set-Cookie` header, `last_login_at` updated, and a redirect to `/` or to the validated `next` path.
- **Throttling.** Five consecutive failures for the same username (case-insensitive) start a 60-second
  cooldown during which every attempt for that username is refused before the password is verified. A
  successful login clears the counter. The window is in memory and per process.
- **Sessions.** A request is authenticated iff its cookie token hashes to a row that is not revoked and
  not expired, and whose user is active. A request whose `last_seen_at` is older than 30 minutes extends
  `expires_at` to `now + TTL` in the same UPDATE that sets `last_seen_at`. Rotation, revocation and expiry
  are never silent: an expired or revoked session is refused exactly like an absent one.
- **Logout.** `POST /logout` and `DELETE /api/sessions` revoke the session when one resolves, always clear
  the cookie, and answer `303` to `/login` (HTML) or `204` (JSON). They never fail on an unknown or
  expired token.
- **Deny by default.** Any request whose path is not in the public allowlist without a valid session is
  refused: `/api/*` answers `401` with `{"error":"unauthorized"}`; an `HX-Request` answers `401` with
  `HX-Redirect: /login` and no body; any other request answers `303` to `/login` (with `next` only when it
  is a local path).
- **Authorization.** A handler that declares `Require<P>` runs only if the principal holds `P`. A refusal
  is `403` and writes nothing, in the shape the caller reads: JSON `{"error": ...}` for `/api/*` and for
  HTMX requests (the global `htmx:responseError` handler renders it as the `#notice` box), and a minimal
  HTML page for a full-page HTML request. The message reaches the operator, so it is written in Spanish.
- **Protected role and last administrator.** The database refuses: deleting a `is_system` role, changing
  its code, deleting its `role_permissions` rows, deactivating the last active user holding it, and
  deleting the last grant of that role to an active user. The interface reports the refusal in Spanish and
  never presents the action as possible.
- **Password change.** `GET /password` renders the change form; `POST /password` requires the current
  password, validates the new one (≥ 12 characters, not equal to the current one, confirmed twice) and
  updates `password_hash`, clears `must_change_password`, and revokes every other session of that user.
  While `must_change_password = 1`, every other non-public route redirects to `/password`.
- **Admin password reset.** `POST /web/users/password` sets a temporary password for another user
  (`identity.users.manage` + audit) and sets `must_change_password = 1` on the target; the resetter never
  sees the target's previous password.
- **Role assignment.** `POST /web/users/roles` replaces a user's role set, recording `granted_by` and
  `granted_at` for the newly granted ones, refusing the removal of the last active protected-role holder,
  and refusing a change that would leave the acting administrator without `identity.roles.manage`.
- **Audit.** Every insert into an audited table writes `created_by = principal.user_id`; every update sets
  `updated_by`. Documents created by another document (a sale's payment, a purchase's payment, a movement
  produced by a sale) carry the acting principal of the originating request, so a flow never invents a
  different actor. The interface shows the display name, never the id.
- **Boundaries.** Identity performs SQL only against identity tables; no department queries identity tables
  or receives the identity service; departments receive a `Principal` value from the kernel.

## Configuration
| Variable | Default | Meaning |
|---|---|---|
| `ROYA_ADMIN_PASSWORD` | unset | Bootstrap administrator password; when absent one is generated and logged once with a forced change |
| `ROYA_SESSION_TTL_HOURS` | `12` | Absolute session lifetime, and the renewal horizon |
| `ROYA_COOKIE_SECURE` | `0` | Adds `Secure` to the session cookie; required when served over HTTPS |
| `ROYA_ALLOWED_ORIGINS` | unset | Comma-separated origins for CORS; unset means same-origin only (no wildcard) |
| `ROYA_LOGIN_THROTTLE_ATTEMPTS` | `5` | Consecutive failures before the cooldown |
| `ROYA_LOGIN_THROTTLE_SECONDS` | `60` | Cooldown length |

## Interface
- Web: `/login` (form), `POST /login`, `POST /logout`, `/password` (change), `/users` (list, create,
  deactivate, reset password, assign roles), `/roles` (list, create, edit, delete, permission matrix).
  Mutations post to collection endpoints with the id in the body, following the existing wiring guard.
- API: `POST /api/sessions` (JSON credentials, sets the session cookie, `204`), `DELETE /api/sessions`
  (revoke, `204`). No REST CRUD for users or roles in v1: the interface is the requirement, and a second
  surface would double the review load.
- Navigation: the sidebar renders the entries the principal may read, plus the current user's display name
  and a logout control.

## Acceptance criteria
- [ ] AC1: with an empty database the application seeds exactly one `admin` user holding the protected
      role; `ROYA_ADMIN_PASSWORD` is honoured, and without it the generated password is logged once and
      the session is confined to the password change until the password is changed.
- [ ] AC2: an anonymous request to any non-public route is refused — `303` to `/login` for HTML, `401`
      JSON for `/api/*`, `401` + `HX-Redirect` for `HX-Request`.
- [ ] AC3: a request to a deliberately unannotated handler is still refused, proving deny-by-default.
- [ ] AC4: unknown username, wrong password and inactive user produce the same message and status, and a
      non-existent username still costs cost-equivalent argon2 work (a full-cost hash of a dummy credential
      with the production parameters).
- [ ] AC5: five consecutive failures throttle the next attempt before verification; a success clears the
      counter; the cooldown expires.
- [ ] AC6: a successful login creates one session row whose `token_hash` is the sha256 of the cookie value
      and whose raw token appears in no table, log or template.
- [ ] AC7: a session whose `expires_at` has passed is refused although the row still exists; a revoked
      session is refused immediately; a session whose owning user has been deactivated is refused exactly
      like an unknown token; revoking twice is idempotent; `revoked_at` cannot be cleared.
- [ ] AC8: activity after 30 idle minutes extends `expires_at`; activity before it does not.
- [ ] AC9: logout clears the cookie, revokes the row, and works with an already-invalid token.
- [ ] AC10: a handler declaring a permission the principal lacks is refused with `403` — JSON for `/api/*`
      and for HTMX, an HTML page for a full-page request — and writes nothing; the same handler runs when the
      permission is granted through any of the user's roles.
- [ ] AC11: effective permissions are the union across roles; a user with no roles has none; editing a
      role's matrix changes the next request without restarting.
- [ ] AC12: the shared catalog test fails if a code exists in one side only (verified by removing a row
      and by removing a catalog entry in throwaway copies).
- [ ] AC13: the protected role cannot be deleted, renamed or re-permissioned, through the interface or
      through direct SQL.
- [ ] AC14: the last active administrator cannot be deactivated and its grant cannot be deleted; a second
      administrator makes the same operation succeed.
- [ ] AC15: a role assigned to a user cannot be deleted, and the interface names the blocking users.
- [ ] AC16: `must_change_password` confines the session to `/password`; a successful change revokes the
      user's other sessions and lifts the restriction.
- [ ] AC17: the permission matrix edits a non-protected role's permissions from the interface and the
      change is visible on the next request.
- [ ] AC18: every mutation of an audited table stores the acting user's id, and the detail view shows the
      display name; a document created inside a flow stores the same actor as the flow's request.
- [ ] AC19: existing rows are backfilled and `created_by` is `NOT NULL` afterwards; a delete of a user
      referenced by an audited row is refused.
- [ ] AC20: no department queries identity tables and no department depends on `IdentityService`
      (verified by grep and by the compile-time wiring).
- [ ] AC21: the navigation shows only the entries the principal may read, and the hidden action is still
      refused at the handler when called directly.
- [ ] AC22: the browser suite covers login, forced password change, session expiry during an HTMX request,
      and a permission-denied HTMX form.
- [ ] AC23: no password, token or hash appears in any response body, log line or template.
- [ ] AC24: the existing HTTP tests authenticate through the shared kernel test helper, and no test-only
      authentication bypass exists in non-test code (verified by grep for a test/flag branch in the guard).
- [ ] AC25: every timestamp the code binds or writes uses the SQLite ISO-Z form
      (`%Y-%m-%dT%H:%M:%S%.3fZ`), proven by a test that crosses the boundary — a column written by the
      database compared against a Rust-bound value of the same instant, in the direction the comparison
      runs. A mixed encoding must fail that test.
