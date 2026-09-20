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
  The comparison covers codes and descriptions: a seeded description must say what the code's gate
  actually allows (the operator reads it in the S4 permission matrix before granting), and a
  one-sided edit of either breaks the test.

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
  `HX-Redirect: /login` and no body; any other request answers `303` to `/login`.
- **`next` is only a local path, and it is encoded.** A request may carry `next` only when it starts with a
  single `/`, is not `//`, contains no backslash and contains no control character at all
  (`char::is_control`, because the URL parser strips TAB/CR/LF before parsing and a raw TAB turns
  `/<TAB>/evil.com` into the network-path-relative `//evil.com`). The value is emitted percent-encoded as a
  query parameter so a filtered page survives the round trip, and every hostile value falls back to `/`.
- **Authorization.** A handler that declares `Require<P>` runs only if the principal holds `P`. A refusal
  is `403` and writes nothing, in the shape the caller reads: JSON `{"error": ...}` for `/api/*` and for
  HTMX requests (the global `htmx:responseError` handler renders it as the `#notice` box), and a minimal
  HTML page for a full-page HTML request. The message reaches the operator, so it is written in Spanish.
- **The declared order rule (double-gated drawers).** A handler that renders two owners' data declares TWO
  `Require<P>` extractors — the product drawer (`/web/products/detail/{id}`: `inventory.read` AND
  `purchases.costs.read`) and the supplier drawer (`/web/suppliers/{id}/detail`: `suppliers.read` AND
  `purchases.costs.read`). The extractors run in DECLARATION ORDER, and the first one to fail produces
  the refusal: which permission the refusal names is therefore decided by the declared order, not by the
  handler's code. The order is part of the gate's contract: the drawer's own-screen read is declared
  first (it is the cheaper, more common miss) and the data-owner's read second, so a principal missing
  either gets named for the one it lacks, in declaration order. A change of order changes which code an
  operator reads in the refusal and is a gate change, not a refactor.
- **Navigation (AC21).** The sidebar renders only the entries the principal may read, and an entry
  declares EVERY permission it needs: the gate of the route its href opens, plus the data-owner
  permission of any page block its label names (the same shape the double-gated drawers and the
  suggestions block already use). A nav entry that declares LESS shows its operator a screen the
  route refuses — the mismatch the 2026-09-20 correction round fixed: `accounts` mapped the
  `/#accounts` entry to `finance.read` while that href opens the `/` route, which declares
  `dashboard.read`, so a principal holding `finance.read` without `dashboard.read` SAW the entry and
  got a 403 clicking it. A nav that declares MORE hides a screen the principal may read. The mapping
  entry → permissions lives in ONE place (`authz::NAV_ENTRIES`, one row per entry: key, the catalog
  codes the entry needs, and the sidebar group); a row may declare no codes for the entries every
  signed-in operator may see (`/password`, the one page a confined session must always reach).
  `accounts` is the worked example of the rule: its href `/#accounts` opens the `/` route (gate
  `dashboard.read`) and its label names the dashboard's accounts block, whose data owner is
  `finance.read` — so the entry carries BOTH codes, and the dashboard renders the accounts block
  conditionally on `finance.read` (the block is a separable card, so the balances need no
  inseparable-from-the-page justification; the rest of the dashboard stays behind `dashboard.read`,
  the gate its own route declares). Every full-page handler builds the sidebar's nav view (`Nav`)
  from its request's principal; the full-page refusal builds the same view from the extensions, so
  the shell around a 403 obeys the same rule and an under-permissioned principal keeps only the
  navigation it may use. The drift tests mirror the permission catalog's: the sidebar partial's
  rendered keys must all be declared (a new entry without a declared mapping fails a test), a
  declared entry must render (no row that decides nothing), and each declared code must be a catalog
  permission. The behavioral truth does NOT trust that table: **for every nav entry, a principal
  holding exactly the permissions that entry declares gets 200 on that entry's href** — one test
  (`ac21_a_principal_holding_exactly_what_an_entry_declares_opens_its_href`) drives the real router
  for every entry, and each declared code must be load-bearing: a principal holding the declared set
  MINUS that code either gets the route's 403 (the code gates the route) or misses the block the
  label names (the code owns the block's data); a code for which neither holds is over-declared and
  the test fails. The one entry this invariant does not reach by permission arithmetic is `password`,
  which declares no code and needs none: it is verified the same way with the permissionless
  (still signed-in) principal. Behaviorally: a limited principal sees
  exactly the entries it may read (no empty group headings either), the bootstrap administrator sees all,
  and the hidden action stays refused at the handler when called directly. The sidebar also shows the
  signed-in user's display name and username next to the logout control, and the password page says why
  it is confining a flagged session.
- **Protected role and last administrator.** The database refuses: deleting a `is_system` role, changing
  its code, deleting its `role_permissions` rows, deactivating the last active user holding it, and
  deleting the last grant of that role to an active user. The interface reports the refusal in Spanish and
  never presents the action as possible.
- **Password change.** `GET /password` renders the change form; `POST /password` requires the current
  password, validates the new one (≥ 12 characters, not equal to the current one, confirmed twice) and
  updates `password_hash`, clears `must_change_password`, and revokes every other session of that user.
  While `must_change_password = 1`, every other non-public route redirects to `/password`. Failed
  current-password attempts share the login's per-username in-memory throttle (same map, same
  `ROYA_LOGIN_THROTTLE_*` config, same injected clock): the confinement must not become a guessing
  machine. A success clears the counter, like a successful login does.
- **Admin password reset.** `POST /web/users/password` sets a temporary password for another user and
  sets `must_change_password = 1` on the target; the resetter never sees the target's previous
  password. The endpoint is gated `identity.users.manage`, and the tier rule is applied in the
  service against the TARGET's roles: resetting the password of a user who holds a protected role
  additionally requires `identity.roles.manage` — taking over an account that administers the
  instance is a decision about the administration — so an actor holding only
  `identity.users.manage` cannot reset a protected holder's password at all, and the takeover path
  (reset another administrator's password, log in as them) does not exist. Resetting oneself through
  this path is refused (that change is `/password`).
- **Role assignment.** `POST /web/users/roles` replaces a user's role set, recording `granted_by` and
  `granted_at` for the newly granted ones, and is gated `identity.roles.manage` — the tier that
  decides who administers the instance (the service repeats the check, so the rule does not depend
  on every future route remembering the extractor). NOBODY changes their own role set, with any
  permission: the rule closes CHANGING YOUR OWN ROLE SET DIRECTLY, and that direct path is where
  both self-escalation and the self-lockout would come from, so one refusal closes them alike; the
  interface hides the action for self while the refusal stays real. The rule does not claim to
  close self-escalation in general: the permission matrix leaves a roles-manage holder one tick
  away from widening a **non-protected** role it occupies (the protected role's matrix cannot be edited by anyone, so a holder whose only role is the protected one has no matrix path at all) — the indirect path and its consequence are written down
  in Cross-account honesty, not denied here. The removal of the last active protected-role
  holder remains refused by the database trigger — the backstop, not a substitute — and the
  submitted role ids are resolved in one statement (one `find_by_ids` call carrying the whole set,
  pinned by a counting-double test), and a body carrying `role_ids` with an empty value is the empty
  set — the well-formed spelling of no roles (the interface omits the key), not a form error; a
  malformed value is still refused. The raw-body handler buffers the form through
  its own limit and answers an oversized body with `413` in the app's Spanish JSON shape, never
  the extractor's English plain-text error.
- **Roles administration (S4).** The screen `/roles` is where the RBAC core becomes editable: the
  list (code, name, description, protected flag, how many users hold it), a create dialog, an edit
  dialog, a delete action and the permission matrix — the 23 seeded codes grouped by module, each
  carrying its seeded Spanish description, the corrected wording the operator reads before
  granting. Saving the matrix REPLACES the role's whole set: the unticked codes are removed, not
  merely left out.
  - **Gating.** Every surface of the screen — reads included — requires `identity.roles.manage`.
    The mapping is deliberate and written down here because the catalog has no
    `identity.roles.read`, and inventing one would need a migration the v1 catalog does not have;
    the cost is stated plainly: an operator who may only LOOK at roles must hold the management
    permission. The handler, not the markup, is the enforcement.
  - **The protected role is presented as locked.** No delete, no code rename, no matrix edit: the
    triggers enforce it, the interface never offers it (the row's button is a read-only "Ver", the
    dialog renders the matrix without checkboxes and without a submit), and the handlers refuse it
    with the reason in Spanish. Its name and description stay editable — the triggers lock the
    code, not the label.
  - **A held role cannot be deleted, and the refusal names the blocking users** (AC15): every user
    holding the role is named in the Spanish conflict, whatever their state — the `user_roles`
    RESTRICT foreign key does not distinguish active from inactive holders, so neither does the
    refusal or the list count.
  - **The matrix self-lockout rule (new in S4).** A matrix edit that would REMOVE
    `identity.roles.manage` from a role the acting principal currently holds is refused, with the
    reason in Spanish and nothing written. Without it, a roles-manage holder could strip its own
    tier through the matrix and lock itself — and everyone sharing the role — out of the
    administration on the very next request: the same lockout class the users screen already
    refuses for role sets. The interface pre-ticks the held permission, so it never offers the
    removal; the refusal is real regardless.
  - **Trigger mappings (ledger closed by S4).** Every trigger refusal reachable from this screen
    maps to its own Spanish conflict, never a generic 500 or a raw trigger string: the protected
    delete (`protected role cannot be deleted`), the protected rename (`protected role code cannot
    change`), the protected matrix removal (`protected role permissions cannot be removed`) and
    the schema CHECK refusals for the role's fields. The create/edit forms validate the same
    rules in the service (code shape `^[a-z][a-z0-9_]*$`, 2-64; name 1-128; description ≤ 256,
    optional), so the CHECKs are backstops for the check-to-write window.
  - **The code is never an editable field.** A machine name is not a relabel: the edit form carries
    name and description only, for every role — so the protected role's rename refusal is not
    reachable through the interface by construction, and the trigger's mapping covers every other
    path (scripts, future screens) that could reach it.
- **Cross-account honesty (what each tier may do to another account).** Whoever holds
  `identity.roles.manage` decides who administers the instance: with it, a principal can grant or
  strip the protected role on any account but its own. While it holds only this permission it
  cannot reset any password — the reset endpoint is gated `identity.users.manage`, and the service
  adds the `roles.manage` requirement only when the target holds a protected role. But one indirect
  path reopens the pair from this tier alone, and it must be read before granting: the matrix
  editor (S4) refuses only REMOVING `identity.roles.manage` from a role the actor holds — it never
  refuses ADDING permissions to a **non-protected** role the actor occupies (the protected role's matrix is refused for every actor, so this path exists only through an ordinary role). A roles-manage holder can therefore
  tick `identity.users.manage` onto a role it holds, get `200`, and hold the pair on the next
  request — and the pair can reset a protected holder's password and take over the
  administration. En palabras del operador: quien puede editar la matriz puede marcarse
  `identity.users.manage` en un rol propio **no protegido**, y con ese par puede restablecer la contraseña de quien
  sostiene un rol protegido y tomar la instancia. Conceder «Crear roles, editar la matriz de
  permisos y cambiar los roles de otras cuentas» no entrega solamente la decisión de quién
  administra: entrega la instancia. Whoever holds `identity.users.manage` can take over the
  accounts it is allowed to touch: create users, activate/deactivate them, edit their display
  name, and reset the password of any user holding NO protected role — an ordinary credential
  takeover of accounts it can already see and manage. Resetting a protected holder's password
  needs both permissions held together, and a roles-manage holder can assemble the pair itself
  through the matrix — so it is not only the pair that hands over the administration:
  `identity.roles.manage` alone already carries the path to it. A permission whose consequence is
  not written down is one an operator cannot grant knowingly: granting `identity.users.manage`
  means handing over the credentials of every non-protected account; granting
  `identity.roles.manage` means handing over the instance itself.
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
  and a logout control. The mapping entry → permission is `authz::NAV_ENTRIES` (one place, drift-tested;
  see the Navigation rule).

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
