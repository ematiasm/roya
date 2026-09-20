# Capability: identity (M5)

> Provenance: promoted from `openspec/changes/2026-09-18-add-identity-module/` (Phase A) on
> delivery, 2026-09-23, and verified claim by claim against the code and the migration chain. The
> audit of the actor on every business table is **in progress, per department**, and this spec states
> the departments that ship today: the finance tables carry it (see "What the actor audit covers
> today"). The remaining departments are planned in
> `openspec/changes/2026-09-19-add-actor-audit/`, and this section is updated as each one lands rather
> than describing the end state ahead of the code. Behaviour stated here is verified by the test suites, not by a reviewed upstream
> proposal.

## Purpose
Who can use the instance and what each one may do: users with credentials, revocable sessions,
roles whose permission matrix is editable from the interface, and a transversal security kernel
that denies by default and authorizes per action.

## Architecture: a transversal kernel

`src/security/` (`password.rs`, `session.rs`, `authz.rs`, `guard.rs`) is a **transversal kernel,
not a department**: no department knows it holds users. A department handler declares the
permission its action needs as an extractor argument (`Require<P>`); the kernel answers. No
department reads identity tables, receives the identity service, or takes any identity dependency —
it receives only an opaque `Principal` value resolved by the kernel per request. `IdentityService`
performs SQL only against identity tables. The permission declared at the route and the code in the
catalog below are one contract: the route table in this spec is the operator-facing face of it.

## Entities

### users
- `id PK`, `username TEXT NOT NULL` (trimmed, 3–64, `^[a-z0-9]([a-z0-9._-]*[a-z0-9])?$`, unique
  case-insensitively through a `COLLATE NOCASE` unique index)
- `display_name TEXT NOT NULL` (≤ 128)
- `password_hash TEXT NOT NULL` (argon2id PHC string; never returned by any read path)
- `is_active INTEGER NOT NULL DEFAULT 1`
- `must_change_password INTEGER NOT NULL DEFAULT 0`
- `last_login_at TEXT NULL`, `created_at`, `updated_at`
- Rules: a username is unique and immutable in practice; users are never deleted, they are
  deactivated; an inactive user cannot log in and cannot hold a session.

### sessions
- `id PK`, `token_hash TEXT NOT NULL UNIQUE` (the sha256 digest of the cookie token,
  base64url-encoded; the token itself is never stored)
- `user_id NOT NULL REFERENCES users(id) ON DELETE CASCADE`
- `created_at`, `expires_at`, `last_seen_at` NOT NULL, `revoked_at TEXT NULL`, `user_agent TEXT NULL` (≤ 256)
- Indexes on `user_id`, `expires_at`.
- Trigger: `revoked_at` cannot go from set to NULL (revocation is permanent).
- Rule: a session is valid iff `revoked_at IS NULL AND expires_at > :now` — decided in SQL, not only
  in Rust.

### roles
- `id PK`, `code TEXT NOT NULL UNIQUE` (`^[a-z][a-z0-9_]*$`), `name TEXT NOT NULL` (the Spanish
  label shown in the interface), `description TEXT NULL` (≤ 256)
- `is_system INTEGER NOT NULL DEFAULT 0` — a protected role
- `created_at`, `updated_at`
- Seed: `admin` (`is_system = 1`, all permissions), plus three ordinary roles `vendedor`, `cajero`,
  `deposito` with their matrices, every insert guarded so re-running cannot duplicate.
- Rules: a protected role cannot be deleted, its `code` cannot change, and its permission rows
  cannot be removed. A role assigned to any user cannot be deleted (FK RESTRICT); the interface
  says which users block it.

### permissions
- `id PK`, `code TEXT NOT NULL UNIQUE` (`<module>.<action>` or `<module>.<resource>.<action>`),
  `module TEXT NOT NULL`, `action TEXT NOT NULL`, `description TEXT NOT NULL`, `created_at`
- Seeded catalog, 23 rows: `dashboard.read`; `finance.read`, `finance.write`,
  `finance.methods.manage`; `inventory.read`, `inventory.write`, `inventory.stock.write`;
  `sales.read`, `sales.create`, `sales.cancel`; `customers.read`, `customers.write`,
  `customers.collect`; `purchases.read`, `purchases.create`, `purchases.cancel`,
  `purchases.costs.read`, `purchases.costs.write`; `suppliers.read`, `suppliers.write`;
  `identity.users.read`, `identity.users.manage`, `identity.roles.manage`.
- Rules: the catalog is data seeded by migration; the application does not create permission rows
  at runtime. A drift test asserts the seeded catalog equals the catalog compiled into
  `security/authz.rs`, **covering codes and descriptions**: a code or a description can exist on
  one side only — and the description must say what the code's gate actually allows, because the
  operator reads it in the roles screen's permission matrix before granting.

### role_permissions
- `role_id NOT NULL REFERENCES roles(id) ON DELETE CASCADE`, `permission_id NOT NULL REFERENCES
  permissions(id) ON DELETE CASCADE`, primary key `(role_id, permission_id)`.

### user_roles
- `user_id NOT NULL REFERENCES users(id) ON DELETE CASCADE`, `role_id NOT NULL REFERENCES roles(id)
  ON DELETE RESTRICT`, `granted_by NOT NULL REFERENCES users(id) ON DELETE RESTRICT`, `granted_at
  NOT NULL`, unique `(user_id, role_id)`.
- The grant trail — who granted, and when — is the actor record this capability owns. Beyond it,
  the audit columns ship per department: `accounts`, `transactions`, `payment_methods`,
  `categories`, `products` and `stock_movements` carry `created_by`/`updated_by` today (see
  "What the actor audit covers today"), and the other departments are planned in
  `openspec/changes/2026-09-19-add-actor-audit/`.

## Derived reads
- `effective_permissions(user) = UNION of the permissions of the user's roles`, resolved per
  request; no cache. Editing a role's matrix changes the next request without a restart.
- `principal(user) = { user_id, username, display_name, must_change_password, effective_permissions }`.
- `is_protected_role(role) = roles.is_system = 1`.
- `active_admin_count = COUNT(users JOIN user_roles JOIN roles WHERE roles.code = 'admin' AND
  users.is_active = 1)` — the quantity the last-administrator triggers protect.
- The permission matrix for the interface: the 23 seeded permissions grouped by `module`, with the
  roles that hold each one.

## Rules

### Authentication
`POST /login` verifies the password against the stored argon2id hash. Unknown username, wrong
password and inactive user all answer `401` with the same message ("Usuario o contraseña
incorrectos"), and a full-cost argon2 hash of a dummy credential is computed even when the username
does not exist, so the answer time does not leak existence. On success: a new session row, the
`Set-Cookie` header, `last_login_at` updated, and a redirect to `/` or to the validated `next` path.

### Throttling
Five consecutive failures for the same username (case-insensitive) start a 60-second cooldown
during which every attempt for that username is refused before the password is verified. A
successful login clears the counter; a failed current-password attempt on `/password` shares the
same per-username throttle (same map, same config, same injected clock), so the confinement cannot
become a guessing machine. The window is in memory and per process.

### Sessions
A request is authenticated iff its cookie token hashes to a row that is not revoked and not
expired, and whose user is active. A request whose `last_seen_at` is older than 30 minutes extends
`expires_at` to `now + TTL` in the same UPDATE that sets `last_seen_at`. Rotation, revocation and
expiry are never silent: an expired or revoked session is refused exactly like an absent one.

### Logout
`POST /logout` and `DELETE /api/sessions` revoke the session when one resolves, always clear the
cookie, and answer `303` to `/login` (HTML) or `204` (JSON). They never fail on an unknown or
expired token.

### Deny by default
Any request whose path is not in the public allowlist (`GET /login`, `POST /login`, `GET
/static/*`, `GET /favicon.ico`, `POST /api/sessions`, `DELETE /api/sessions`) without a valid
session is refused: `/api/*` answers `401` with `{"error":"unauthorized"}`; an `HX-Request` answers
`401` with `HX-Redirect: /login` and no body (htmx 1.9.12 performs that navigation); any other
request answers `303` to `/login`. A handler with no permission declaration is still refused by the
session gate — deny-by-default does not depend on annotations. Unsafe HTTP methods are additionally
refused cross-origin: an `Origin` that does not match the `Host` is rejected before anything else,
`/login` included.

### `next` is only a local path, and it is encoded
A request may carry `next` only when it starts with a single `/`, is not `//`, contains no
backslash and contains no control character at all (`char::is_control` — the URL parser strips
TAB/CR/LF before parsing, and a raw TAB turns `/<TAB>/evil.com` into the network-path-relative
`//evil.com`; this correction round followed an adversarial review that found the exploitable open
redirect). The value is emitted percent-encoded as a query parameter so a filtered page survives the
round trip, and every hostile value falls back to `/`.

### Authorization
A handler that declares `Require<P>` runs only if the principal holds `P`. A refusal is `403` and
writes nothing, in the shape the caller reads: JSON `{"error": ...}` for `/api/*` and for HTMX
requests (the global `htmx:responseError` handler renders it as the `#notice` box), and a minimal
HTML page for a full-page HTML request. The message reaches the operator, so it is written in
Spanish.

### The declared order rule (double-gated drawers)
A handler that renders two owners' data declares TWO `Require<P>` extractors — the product drawer
(`/web/products/detail/{id}`: `inventory.read` AND `purchases.costs.read`) and the supplier drawer
(`/web/suppliers/{id}/detail`: `suppliers.read` AND `purchases.costs.read`). The extractors run in
DECLARATION ORDER, and the first one to fail produces the refusal: which permission the refusal
names is decided by the declared order, not by the handler's code. The drawer's own-screen read is
declared first (the cheaper, more common miss) and the data-owner's read second. A change of order
changes which code an operator reads in the refusal and is a gate change, not a refactor.

### Navigation (AC21)
The sidebar renders only the entries the principal may read, and an entry declares EVERY permission
it needs: the gate of the route its href opens, plus the data-owner permission of any page block
its label names. A nav entry that declares LESS shows its operator a screen the route refuses; one
that declares MORE hides a screen the principal may read. The mapping entry → permissions lives in
ONE place (`authz::NAV_ENTRIES`, one row per entry: key, catalog codes, sidebar group); the
`password` entry declares no codes — it is the one page a confined session must always reach.
`accounts` is the worked example: its href `/#accounts` opens the `/` route (gate `dashboard.read`)
and its label names the dashboard's accounts block, whose data owner is `finance.read` — the entry
carries BOTH codes, and the dashboard renders the accounts block conditionally on `finance.read`.
Every full-page handler builds the sidebar's nav view from its request's principal; the full-page
refusal builds the same view from the extensions, so the shell around a 403 obeys the same rule.
The behavioral truth does not trust the table: for every nav entry, a principal holding exactly the
permissions that entry declares gets 200 on that entry's href (one test, every entry, driven
against the real router), and each declared code must be load-bearing — the declared set minus that
code either gets the route's 403 or misses the block the label names. The sidebar also shows the
signed-in user's display name and username next to the logout control, and the password page says
why it is confining a flagged session.

### Protected role and last administrator
The database refuses, through triggers in the guard migration: deleting a `is_system` role,
changing its code, deleting its `role_permissions` rows, deactivating the last active user holding
it, and deleting the last grant of that role to an active user. The interface reports the refusal
in Spanish and never presents the action as possible.

### Password change
`GET /password` renders the change form; `POST /password` requires the current password, validates
the new one (≥ 12 characters, not equal to the current one, confirmed twice) and updates
`password_hash`, clears `must_change_password`, and revokes every other session of that user (one
UPDATE; the acting session keeps its id and expiry). While `must_change_password = 1`, every other
non-public route is confined to `/password`: a full-page request answers `303`, `/api/*` a `403`
JSON reason, an `HX-Request` the refusal with `HX-Redirect: /password`. Failed current-password
attempts share the login's throttle (see above); a success clears the counter, like a successful
login does.

### The tiered users administration
`/users` is the administration screen: list with roles and state, create, deactivate/activate,
admin password reset, role assignment. Reads are gated `identity.users.read` and the mutations
`identity.users.manage`. The service owns the tier rules, so they do not depend on every future
route remembering the extractor:
- **Admin password reset.** `POST /web/users/password` sets a temporary password for another user
  and sets `must_change_password = 1` on the target; the resetter never sees the target's previous
  password. The tier rule is applied against the TARGET's roles: resetting the password of a user
  who holds a protected role additionally requires `identity.roles.manage` — taking over an account
  that administers the instance is a decision about the administration — so a principal holding only
  `identity.users.manage` cannot reset a protected holder's password at all, and the takeover path
  does not exist. Resetting oneself through this path is refused (that change is `/password`).
- **Role assignment.** `POST /web/users/roles` replaces a user's role set, recording `granted_by`
  and `granted_at` for the newly granted ones, and is gated `identity.roles.manage`. NOBODY changes
  their own role set, with any permission: one refusal closes self-escalation and the self-lockout
  on the direct path. The interface hides the action for self while the refusal stays real. The
  submitted role ids are resolved in one statement, and a body carrying `role_ids` with an empty
  value is the empty set — the well-formed spelling of no roles, not a form error; a malformed value
  is still refused. The raw-body handler buffers the form through its own limit and answers an
  oversized body with `413` in the app's Spanish JSON shape.
- **Deactivation revokes the deactivated user's sessions**; the last active protected-role holder
  cannot be deactivated (the trigger refuses — the backstop, not a substitute).

### Roles administration
The screen `/roles` is where the RBAC core becomes editable: the list (code, name, description,
protected flag, how many users hold it), a create dialog, an edit dialog, a delete action and the
permission matrix — the 23 seeded codes grouped by module, each carrying its seeded Spanish
description. Saving the matrix REPLACES the role's whole set: the unticked codes are removed, not
merely left out.
- **Gating.** Every surface of the screen — reads included — requires `identity.roles.manage`. The
  mapping is deliberate: the catalog has no `identity.roles.read`, and inventing one would need a
  migration; the cost is stated plainly — an operator who may only LOOK at roles must hold the
  management permission. The handler, not the markup, is the enforcement.
- **The protected role is presented as locked.** No delete, no code rename, no matrix edit: the
  triggers enforce it, the interface never offers it. Its name and description stay editable — the
  triggers lock the code, not the label. The code is never an editable field for any role: the edit
  form carries name and description only.
- **A held role cannot be deleted, and the refusal names the blocking users** (AC15): every user
  holding the role is named in the Spanish conflict, whatever their state — the `user_roles`
  RESTRICT foreign key does not distinguish active from inactive holders, so neither does the
  refusal or the list count.
- **The matrix self-lockout rule.** A matrix edit that would REMOVE `identity.roles.manage` from a
  role the acting principal currently holds is refused, with the reason in Spanish and nothing
  written. The interface pre-ticks the held permission, so it never offers the removal; the refusal
  is real regardless.
- **Trigger mappings.** Every trigger refusal reachable from this screen maps to its own Spanish
  conflict, never a generic 500 or a raw trigger string: the protected delete, the protected rename,
  the protected matrix removal, and the schema CHECK refusals for the role's fields. The
  create/edit forms validate the same rules in the service, so the CHECKs are backstops for the
  check-to-write window.

### Cross-account honesty (what each tier may do to another account)
Whoever holds `identity.roles.manage` decides who administers the instance: with it, a principal
can grant or strip the protected role on any account but its own. While it holds only this
permission it cannot reset any password — the reset endpoint is gated `identity.users.manage`, and
the service adds the `roles.manage` requirement only when the target holds a protected role. But
one indirect path reopens the pair from this tier alone, and it must be read before granting: the
matrix editor refuses only REMOVING `identity.roles.manage` from a role the actor holds — it never
refuses ADDING permissions to a **non-protected** role the actor occupies. A roles-manage holder can
therefore tick `identity.users.manage` onto a role it holds, get `200`, and hold the pair on the
next request — and the pair can reset a protected holder's password and take over the
administration. A permission whose consequence is not written down is one an operator cannot grant
knowingly: granting `identity.users.manage` means handing over the credentials of every
non-protected account; granting `identity.roles.manage` means handing over the instance itself.

### What the actor audit covers today
`user_roles` records `granted_by` and `granted_at` for every role grant, and every mutation of
`accounts`, `transactions`, `payment_methods`, `categories`, `products` and `stock_movements`
records its actor in `created_by` (NOT NULL, `ON DELETE RESTRICT` to `users`) and `updated_by`,
written from the request's principal — never from anything the request itself can supply — and
shown as a name in the account views and the product detail / stock list. `product_barcodes`
carries no columns of its own: a join row inherits the actor of its parent product, as the plan's
Audit section states for lines and join rows.

A movement produced INSIDE another document carries the flow's request actor: a sale or purchase
confirm (or cancel) stamps the stock movements with the same acting user that stamps the flow's
finance rows — the flow never invents a fresh actor (AC18).

Rows that predate the audit — the five seeded payment methods and any historical business row — are
attributed to the inactive, roleless sentinel account `sistema` ("Sistema (anterior al registro)"),
which the migration creates when there is something to attribute. It is deliberate that this is
**not** the bootstrap administrator: those rows were not created by a person the system knew, and
attributing them to one would invent history. The sentinel consumes `users.id = 1` on a fresh
install because the seeded payment methods are rows the audit must attribute; it cannot log in (an
unusable credential and an inactive state, two independent guards) and appears in the users list as
an inactive account, which is where the attribution is explained rather than hidden. The inventory
migration reuses that same sentinel — its own guarded insert is defensive, firing only if the
account is somehow absent when there are inventory rows to attribute.

Every remaining department's tables record no actor yet: the audit there is Phase B (see the
provenance note).

### Boundaries
Identity performs SQL only against identity tables; no department queries identity tables or
receives the identity service; departments receive a `Principal` value from the kernel.

## Configuration
| Variable | Default | Meaning |
|---|---|---|
| `ROYA_ADMIN_PASSWORD` | unset | Bootstrap administrator password; when absent one is generated and logged once with a forced change |
| `ROYA_SESSION_TTL_HOURS` | `12` | Absolute session lifetime, and the renewal horizon |
| `ROYA_COOKIE_SECURE` | `false` | Adds `Secure` to the session cookie when set to `true` or `1`; required when served over HTTPS |
| `ROYA_ALLOWED_ORIGINS` | unset | Comma-separated origins for CORS; unset means same-origin only (no wildcard) |
| `ROYA_LOGIN_THROTTLE_ATTEMPTS` | `5` | Consecutive failures before the cooldown |
| `ROYA_LOGIN_THROTTLE_SECONDS` | `60` | Cooldown length |

## Interface
- Web: `/login` (form), `POST /login`, `POST /logout`, `/password` (change), `/users` (list, create,
  deactivate, reset password, assign roles), `/roles` (list, create, edit, delete, permission
  matrix). Mutations post to collection endpoints with the id in the body, following the existing
  wiring guard.
- API: `POST /api/sessions` (JSON credentials, sets the session cookie, `204`), `DELETE /api/sessions`
  (revoke, `204`). No REST CRUD for users or roles: the interface is the requirement, and a second
  surface would double the review load.
- Navigation: the sidebar renders the entries the principal may read, plus the current user's
  display name and username and a logout control. The mapping entry → permissions is
  `authz::NAV_ENTRIES` (one place, drift-tested; see the Navigation rule).

## Route → permission mapping

The operator-facing contract. Every gate is a per-handler `Require<P>` extractor; there is no
router-level guard, because the modules mix read/write/cancel on the same paths. Codes are the
seeded catalog codes. The table is verified against the handlers by the enforcement tests; the
route's refusal shape is the one the Authorization rule states.

Public (no session needed): `GET /login`, `POST /login`, `GET /static/*`, `GET /favicon.ico`,
`POST /api/sessions`, `DELETE /api/sessions`.

Any signed-in operator (no permission): `GET /password`, `POST /password` (self change; the
service still requires the current password).

### Dashboard and finance — `src/routes/web.rs`, `src/routes/api.rs`

| Route | Method | Permission |
| --- | --- | --- |
| `/` | GET | `dashboard.read` |
| `/accounts/{id}` | GET | `finance.read` |
| `/accounts/{id}/payment-methods` | POST | `finance.methods.manage` |
| `/web/accounts` | GET | `finance.read` |
| `/web/accounts` | POST | `finance.methods.manage` |
| `/web/account-options` | GET | `finance.read` |
| `/web/transactions` | GET | `finance.read` |
| `/web/transactions` | POST | `finance.write` |
| `/web/transactions/{id}` | DELETE | `finance.write` |
| `/api/accounts` | GET | `finance.read` |
| `/api/accounts` | POST | `finance.methods.manage` |
| `/api/accounts/{id}` | GET | `finance.read` |
| `/api/payment-methods` | GET | `finance.read` |
| `/api/accounts/{id}/payment-methods` | GET | `finance.methods.manage` |
| `/api/accounts/{id}/payment-methods` | PUT | `finance.methods.manage` |
| `/api/transactions` | GET | `finance.read` |
| `/api/transactions` | POST | `finance.write` |
| `/api/transactions/{id}` | PUT | `finance.write` |
| `/api/transactions/{id}` | DELETE | `finance.write` |

### Inventory — `src/routes/inventory_api.rs`, `src/routes/inventory_web.rs`

| Route | Method | Permission |
| --- | --- | --- |
| `/api/categories` | GET | `inventory.read` |
| `/api/categories` | POST | `inventory.write` |
| `/api/categories/{id}` | GET | `inventory.read` |
| `/api/categories/{id}` | PUT | `inventory.write` |
| `/api/categories/{id}` | DELETE | `inventory.write` |
| `/api/products` | GET | `inventory.read` |
| `/api/products` | POST | `inventory.write` |
| `/api/products/{id}` | GET | `inventory.read` |
| `/api/products/{id}` | PUT | `inventory.write` |
| `/api/products/{id}` | DELETE | `inventory.write` |
| `/api/products/{id}/stock` | GET | `inventory.read` |
| `/api/products/{id}/barcodes` | GET | `inventory.read` |
| `/api/products/{id}/barcodes` | POST | `inventory.write` |
| `/api/stock-movements` | GET | `inventory.read` |
| `/api/stock-movements` | POST | `inventory.stock.write` |
| `/api/low-stock` | GET | `inventory.read` |
| `/api/negative-stock` | GET | `inventory.read` |
| `/products` | GET | `inventory.read` |
| `/web/products` | GET | `inventory.read` |
| `/web/products` | POST | `inventory.write` |
| `/web/categories` | POST | `inventory.write` |
| `/web/category-options` | GET | `inventory.read` |
| `/web/product-options` | GET | `inventory.read` |
| `/web/product-search` | GET | `inventory.read` |
| `/web/products/detail/{id}` | GET | `inventory.read` AND `purchases.costs.read` (two extractors) |
| `/web/products/edit` | POST | `inventory.write` |
| `/web/products/activate` | POST | `inventory.write` |
| `/web/products/deactivate` | POST | `inventory.write` |
| `/web/products/delete` | POST | `inventory.write` |
| `/web/product-costs` | POST | `purchases.costs.write` |
| `/web/product-costs/preferred` | POST | `purchases.costs.write` |
| `/web/stock-movements` | POST | `inventory.stock.write` |
| `/web/low-stock` | GET | `inventory.read` |
| `/web/negative-stock` | GET | `inventory.read` |

### Sales — `src/routes/sales_api.rs`, `src/routes/sales_web.rs`

| Route | Method | Permission |
| --- | --- | --- |
| `/api/sales` | GET | `sales.read` |
| `/api/sales/debt` | GET | `sales.read` |
| `/api/sales/{id}` | GET | `sales.read` |
| `/api/sales` | POST | `sales.create` |
| `/api/sales/{id}` | PUT | `sales.create` |
| `/api/sales/{id}/lines` | POST | `sales.create` |
| `/api/sales/lines/{line_id}` | PUT | `sales.create` |
| `/api/sales/lines/{line_id}` | DELETE | `sales.create` |
| `/api/sales/{id}/payments` | POST | `customers.collect` |
| `/api/sales/{id}/confirm` | POST | `sales.create` |
| `/api/sales/{id}/cancel` | POST | `sales.cancel` |
| `/sales` | GET | `sales.read` |
| `/sales/{id}` | GET | `sales.read` |
| `/web/sales` | GET | `sales.read` |
| `/web/sales` | POST | `sales.create` |
| `/web/sales/debt` | GET | `sales.read` |
| `/web/sales/lines` | POST | `sales.create` |
| `/web/sales/confirm` | POST | `sales.create` |
| `/web/sales/payments` | POST | `customers.collect` |
| `/web/sales/cancel` | POST | `sales.cancel` |
| `/web/sales/{id}` | GET | `sales.read` |
| `/web/sales/{id}/lines` | POST | `sales.create` |
| `/web/sales/{sale_id}/lines/{line_id}` | POST | `sales.create` |
| `/web/sales/{sale_id}/lines/{line_id}` | DELETE | `sales.create` |
| `/web/sales/{id}/header` | POST | `sales.create` |
| `/web/sales/{id}/confirm` | POST | `sales.create` |
| `/web/sales/{id}/payments` | POST | `customers.collect` |
| `/web/sales/{id}/cancel` | POST | `sales.cancel` |

### Customers — `src/routes/customers_api.rs`, `src/routes/customers_web.rs`

| Route | Method | Permission |
| --- | --- | --- |
| `/api/customers` | GET | `customers.read` |
| `/api/customers` | POST | `customers.write` |
| `/api/customers/ageing` | GET | `customers.read` |
| `/api/customers/{id}` | GET | `customers.read` |
| `/api/customers/{id}` | PUT | `customers.write` |
| `/api/customers/{id}` | DELETE | `customers.write` |
| `/api/customers/{id}/statement` | GET | `customers.read` |
| `/api/customers/{id}/activate` | POST | `customers.write` |
| `/api/customers/{id}/deactivate` | POST | `customers.write` |
| `/api/customer-receipts` | GET | `customers.read` |
| `/api/customer-receipts` | POST | `customers.collect` |
| `/api/customer-receipts/{id}` | GET | `customers.read` |
| `/customers` | GET | `customers.read` |
| `/customers/{id}` | GET | `customers.read` |
| `/web/customers` | GET | `customers.read` |
| `/web/customers` | POST | `customers.write` |
| `/web/customers/edit` | POST | `customers.write` |
| `/web/customers/activate` | POST | `customers.write` |
| `/web/customers/deactivate` | POST | `customers.write` |
| `/web/customers/delete` | POST | `customers.write` |
| `/web/customers/detail/{id}` | GET | `customers.read` |
| `/web/customers/edit-form/{id}` | GET | `customers.read` |
| `/web/customers/{id}/receipts` | GET | `customers.read` |
| `/web/customer-receipts` | POST | `customers.collect` |

### Purchases — `src/routes/purchases_api.rs`, `src/routes/purchases_web.rs`

| Route | Method | Permission |
| --- | --- | --- |
| `/api/purchases` | GET | `purchases.read` |
| `/api/purchases/suggestions` | GET | `inventory.read` |
| `/api/purchases` | POST | `purchases.create` |
| `/api/purchases/{id}` | GET | `purchases.read` |
| `/api/purchases/{id}` | PUT | `purchases.create` |
| `/api/purchases/{id}/lines` | POST | `purchases.create` |
| `/api/purchases/lines/{line_id}` | PUT | `purchases.create` |
| `/api/purchases/lines/{line_id}` | DELETE | `purchases.create` |
| `/api/purchases/{id}/payments` | POST | `purchases.create` |
| `/api/supplier-payments` | POST | `purchases.create` |
| `/api/purchases/{id}/confirm` | POST | `purchases.create` |
| `/api/purchases/{id}/cancel` | POST | `purchases.cancel` |
| `/api/suppliers` | GET | `suppliers.read` |
| `/api/suppliers` | POST | `suppliers.write` |
| `/api/suppliers/{id}` | GET | `suppliers.read` |
| `/api/suppliers/{id}` | PUT | `suppliers.write` |
| `/api/suppliers/{id}` | DELETE | `suppliers.write` |
| `/api/suppliers/{id}/activate` | POST | `suppliers.write` |
| `/api/suppliers/{id}/deactivate` | POST | `suppliers.write` |
| `/api/product-supplier-costs` | GET | `purchases.costs.read` |
| `/api/product-supplier-costs` | POST | `purchases.costs.write` |
| `/purchases` | GET | `purchases.read` |
| `/purchases/{id}` | GET | `purchases.read` |
| `/web/purchases` | GET | `purchases.read` |
| `/web/purchases` | POST | `purchases.create` |
| `/web/purchases/suggestions` | GET | `inventory.read` |
| `/web/purchases/from-suggestion` | POST | `purchases.create` |
| `/web/purchases/lines` | POST | `purchases.create` |
| `/web/purchases/confirm` | POST | `purchases.create` |
| `/web/purchases/payments` | POST | `purchases.create` |
| `/web/purchases/cancel` | POST | `purchases.cancel` |
| `/web/purchases/{id}` | GET | `purchases.read` |
| `/web/purchases/{id}/lines` | POST | `purchases.create` |
| `/web/purchases/{purchase_id}/lines/{line_id}` | POST | `purchases.create` |
| `/web/purchases/{purchase_id}/lines/{line_id}` | DELETE | `purchases.create` |
| `/web/purchases/{id}/header` | POST | `purchases.create` |
| `/web/purchases/{id}/confirm` | POST | `purchases.create` |
| `/web/purchases/{id}/payments` | POST | `purchases.create` |
| `/web/purchases/{id}/cancel` | POST | `purchases.cancel` |

### Suppliers — `src/routes/suppliers_web.rs` (REST twins live in `purchases_api.rs` above)

| Route | Method | Permission |
| --- | --- | --- |
| `/suppliers` | GET | `suppliers.read` |
| `/web/suppliers` | GET | `suppliers.read` |
| `/web/suppliers` | POST | `suppliers.write` |
| `/web/suppliers/edit` | POST | `suppliers.write` |
| `/web/suppliers/{id}` | DELETE | `suppliers.write` |
| `/web/suppliers/{id}/detail` | GET | `suppliers.read` AND `purchases.costs.read` (two extractors) |
| `/web/suppliers/{id}/edit-form` | GET | `suppliers.read` |
| `/web/suppliers/{id}/activate` | POST | `suppliers.write` |
| `/web/suppliers/{id}/deactivate` | POST | `suppliers.write` |
| `/web/supplier-costs` | POST | `purchases.costs.write` |
| `/web/supplier-payments` | POST | `purchases.create` |

### Users and roles — `src/routes/users_web.rs`, `src/routes/roles_web.rs`

| Route | Method | Permission |
| --- | --- | --- |
| `/users` | GET | `identity.users.read` |
| `/web/users` | GET | `identity.users.read` |
| `/web/users/password-form/{id}` | GET | `identity.users.manage` |
| `/web/users` | POST | `identity.users.manage` |
| `/web/users/activate` | POST | `identity.users.manage` |
| `/web/users/deactivate` | POST | `identity.users.manage` |
| `/web/users/password` | POST | `identity.users.manage` |
| `/web/users/roles-form/{id}` | GET | `identity.roles.manage` |
| `/web/users/roles` | POST | `identity.roles.manage` |
| `/roles` | GET | `identity.roles.manage` |
| `/web/roles` | GET | `identity.roles.manage` |
| `/web/roles/edit-form/{id}` | GET | `identity.roles.manage` |
| `/web/roles` | POST | `identity.roles.manage` |
| `/web/roles/edit` | POST | `identity.roles.manage` |
| `/web/roles/delete` | POST | `identity.roles.manage` |
| `/web/roles/matrix` | POST | `identity.roles.manage` |

Mapping decisions the operator should know (all deliberate, fail-closed):
- **Account creation and method allowlisting are `finance.methods.manage`, not `finance.write`**:
  the ledger structure is the manage tier. Known consequence: a principal holding ONLY
  `finance.methods.manage` is refused `GET /api/payment-methods` — the catalog of the very methods
  it may administer. No seeded matrix separates the codes, so no operator hits it today; if a
  future one does, the fix is an OR of the two codes on that one read, not a new kernel type.
- **A payment on a sale is `customers.collect`, not `sales.create`**: money received against an
  owed balance is a collection. Consequence: a principal holding `sales.create` WITHOUT
  `customers.collect` cannot register a payment on the sale it recorded. No seeded matrix separates
  the pair.
- **A payment to a supplier is `purchases.create`, not `suppliers.write`**: the payment is a
  purchase-side movement; entity editing must not hand over money movements. Consequence: a
  `suppliers.write`-only principal sees the pay card in the drawer it may open and is refused on
  submit, naming `purchases.create`. No seeded role holds `suppliers.write` at all.
- **The reorder suggestions are `inventory.read`**: stock-derived data, not a purchase document. A
  `purchases.read`-only principal is refused the suggestions fragment; the `/purchases` page
  renders the Sugerido block conditionally on `inventory.read` so it shows no data its own refresh
  button would refuse, while the supplier roster embedded in the create dialog stays a recorded
  consequence the dialog needs.
- **The reorder-suggestion data reaching `/purchases` is embedded server-side** (`inventory.read`
  and `suppliers.read` data): a purchases-only principal sees that embedded context, but the
  suggestion fragment and the supplier screens themselves refuse it.
- **Cross-capability pairs:** creating a sale needs a customer, and the record page's product
  picker reads `/web/product-search` (`inventory.read`); the customer statement is a single
  `customers.read` gate — a `customers.read`-only principal sees THAT customer's own sale documents
  (number/date/total), never the sales list or another customer's sales. The sale-debt page is
  `sales.read` (unpaid SALES, not customer data).
- The drawer fragments carry the double gate; the declared order rule above decides which code the
  refusal names.

## Verification
`src/security/` (the kernel's own tests, including the catalog drift test and the AC21 nav
invariants), the enforcement tests in the route modules (every gate mutation-validated: the gate
removed, its test observed failing, the gate restored), `src/smoke_tests.rs`, and the AC22 browser
cases in `e2e/tests/test_identity.py` (login, forced password change, session expiry mid-HTMX,
permission-denied HTMX form). The protected-role and last-administrator guarantees are proven at
the database level by direct-SQL tests against the triggers.
