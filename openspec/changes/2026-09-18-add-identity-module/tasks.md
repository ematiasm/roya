# Tasks: add-identity-module

## Review Workload Forecast
- Estimated: ~7,000-7,700 lines across 15 slices. Phase A (authorization) ~5,500 in 9 slices — S1a came
  in at ~2,500 lines (kernel core: 2,313 new + 201 modified, with roughly a third of it tests), S1b ~900
  dominated by mechanical test-cookie plumbing, and S2-S8 ~2,000 together. Phase B (audit) ~1,900 in 6
  slices. Slices are chained branches, one PR each.
- Chained PRs recommended: **Yes — every slice.**
- 400-line budget risk: **High** for every slice, and extreme if Phase A were attempted as one PR.
- Decision needed before apply: **Yes** — the user decides whether Phase B runs immediately after Phase A
  or as a separate program.

## Phase A — authorization

### S1a — identity kernel core (additive, router untouched)
- [x] T1: dependencies (`argon2`, and the already-locked `sha2`, `getrandom`, `base64`), `security/password.rs`
      with the production parameters and a light test hasher, plus the test that pins the production cost.
- [x] T2: migrations `create_identity_users` and `create_identity_sessions` with the triggers.
- [x] T3: models `User` (no hash), `UserWithHash` (authentication only), `NewUser`, `Session`, `NewSession`,
      plus the `error.rs` variants for unauthorized (401) and forbidden (403), mapping to the existing
      JSON response shape.
- [x] T4: `UserRepository` and `SessionRepository` traits and SQLite impls (validity decided in SQL).
- [x] T5: `AuthService` (the identity service): bootstrap admin, login with constant-time verification and
      the generic failure, in-memory throttle with an injected clock, session mint/resolve/renew/revoke,
      logout. Tests for AC1, AC4-AC9, AC23.

S1a closed by three verification rounds: 359 baseline → 410 tests, 0 failures. Round one found two tests that
could not detect the property they named plus the timestamp-encoding bug behind `prune` (now AC25); round two
found a throttle-cap regression introduced by round one's fix; round three broke each fix on purpose and
confirmed every one of them fails when reverted. See `odd/tasks/identity-rbac.md` for the mutation tables and
the residual risks S1b inherits.

### S1b — the wiring (the one unavoidably large slice, split in two)
The combined slice exceeded what one writer run could carry (the first attempt failed having written nothing),
so it is split on the review-friendly seam: the mechanical test plumbing lands first, while nothing enforces
anything, and the behaviour change second, when the tests already authenticate.

#### S1b-i — test-session plumbing (no behaviour change)
- [x] `security/test_support.rs`: a fixed test session token, the cookie constant, `seed_session` through the
      real repositories, `with_cookie`, and a test proving `resolve_session` resolves it.
- [x] Every route test module plus `src/smoke_tests.rs`: the session is seeded in `test_state()` and the
      cookie rides in the local request helpers. No assertion changes; the suite stays at 410.

#### S1b-ii — the gate
- [x] T6: `security/guard.rs` middleware with the public allowlist, the three refusal shapes and the origin
      check on unsafe methods.
- [x] T6b: remove every temporary `#[allow(dead_code)]` / `#[allow(unused_imports)]` attribute that S1a
      added (15 of them, in `security/mod.rs`, `repositories/{user,session}_repo.rs`, `services/identity.rs`,
      `models.rs`, `error.rs`, `repositories/mod.rs`, `services/mod.rs`): once the router consumes the kernel
      they are no longer honest, and the slice is not done until `grep -rn 'allow(dead_code)\|allow(unused_imports)'
      src/security src/services/identity.rs src/repositories/user_repo.rs src/repositories/session_repo.rs`
      returns only genuinely justified entries (S1a suppressed 51 bin warnings with them, so leaving them is
      leaving 51 warnings hidden). The `AppError::Forbidden` one stays until S2 constructs it.
- [x] T7: `identity_web.rs` (`GET /login`, `POST /login`, `POST /logout`), the login template, the sidebar
      logout control, `AppState` and `main.rs` wiring, environment variables, CORS narrowed from the wildcard,
      and the bootstrap call. `must_change_password` is deliberately NOT enforced yet (its route arrives in
      S3, and enforcing the confinement now would lock the bootstrap administrator out).
- [x] T8: the gate's own tests — AC2 in its three shapes, AC3 over a representative protected-route list,
      AC24 (an app built through the shared test helper still refuses a request with no cookie), the origin
      check, a full login round trip, `next` validation against an external URL, and that
      `must_change_password` is not enforced yet; plus the end-to-end `curl` probe against the real binary
      (303 to `/login`, 401 for `/api/*`, cookie set on a correct password, 200 with the cookie, logout,
      dead cookie refused).

S1b-ii went through an adversarial verification that returned DO NOT COMMIT: an exploitable open redirect
(CWE-601) hid behind a `next` validator that rejected `\`, `\r` and `\n` but not TAB, so `/<TAB>/evil.com`
reached the `Location` header raw and Chromium collapsed it to `//evil.com` off-origin. The correction round
replaced the enumeration with `char::is_control()`, percent-encodes `next` as a query parameter, added a
12-entry hostile table at both the unit and end-to-end levels, restored the wiring guard's route-existence
oracle for `POST /logout` (an anonymous probe could not tell a refusal from a registered route), and added an
HTTP-level anonymous `/static/*` test. 433 -> 439 tests; the browser half of the attack was re-run in real
Chromium against the real binary with a decoy server that received no request at all.

#### S1b-iii — the JSON session API and the second test layer
- [ ] T9: `src/routes/identity_api.rs` (`POST /api/sessions` with JSON credentials answering `204` +
      `Set-Cookie`, `DELETE /api/sessions` revoking and clearing), its route tests, and the allowlist entry
      in the guard that was left waiting for it.
- [ ] T9b: `e2e/conftest.py` + `e2e/helpers.py` — the browser suite cannot authenticate once the gate
      lands, so the harness logs in once per server and hands the cookie to both clients (`urllib` requests
      and `context.add_cookies`), instead of every test logging in.
- [ ] T9c: `README.md` (the no-auth section, migrations list, environment table, project structure) and
      `env.example` for the new variables.

### S2 — RBAC core
- [ ] T9: migration `create_identity_rbac` (catalog, seeded roles, guarded inserts) and
      `create_identity_guards` (the lockout triggers).
- [ ] T10: `security/authz.rs`: `Principal`, the permission catalog in code, the `Permission` trait and
      marker types, `Require<P>` extractor, 403 in both shapes.
- [ ] T11: role and permission repositories, `IdentityService` permission resolution per request, and the
      catalog-drift test.
- [ ] T12: tests for AC10-AC12, AC19 (partial: the identity side), AC20.

#### S2 warning ledger (recorded 2026-09-19; updated in the guard-hardening correction round; consumed by S3 part 1 on 2026-09-20)
`cargo check --all-targets` measured **58 warnings on `main`** and **68 after S2** (unchanged by the
guard-hardening round: its triggers and tests replace one another one for one, and the bootstrap wiring
made `role_repo::count_active_protected_holders` reachable, moving it out of this list while `revoke`
joined the S3 side). No
`#[allow(dead_code)]` / `#[allow(unused_imports)]` attributes were added;
`grep -rn 'allow(dead_code)\|allow(unused_imports)' src/` stays empty. The +10 delta is
surface S2 ships ahead of its consumers (the bootstrap protected-role grant and the
middleware's effective-permission resolution are wired and reachable; what remains is
dormant by design, exercised by tests, awaiting its consuming slice):

| Dormant item | Location | Consumed by |
| --- | --- | --- |
| `Principal` identity-field reads (`user_id`, `username`, `display_name`, `must_change_password`) | `src/security/authz.rs:141` | S5-S7 (navigation, forced-change gate) |
| `Principal::has` / `has_permission` | `src/security/authz.rs:164` | S5 (the extractor's membership check) |
| `Require<P>` construction | `src/security/authz.rs:189` | S5-S7 (handler declarations) |
| `forbidden_response` | `src/security/authz.rs:233` | S5-S7 (the extractor's rejection) |
| `ForbiddenTemplate` | `src/security/authz.rs:262` | S5-S7 (full-page refusal) |
| `Role` field reads (`code`, `name`, `description`, `is_system`, timestamps) | `src/models.rs:1346` | S3 (users list), S4 (roles list) |
| `Permission` model struct | `src/models.rs:1359` | S4 (permission matrix rows) |
| `permission_repo::map_db_err` | `src/repositories/permission_repo.rs:24` | S4 (matrix editor refusals) |
| `permission_repo::{list, codes_for_role, set_role_permissions}` | `src/repositories/permission_repo.rs:40` | S4 (matrix editor) |
| `role_repo::{find_by_id, list, list_for_user, count_active_holders, revoke, replace_user_roles}` | `src/repositories/role_repo.rs:54` | S3 (assignment form, deactivation), S4 (roles admin) |
| `user_repo::find_by_username` | `src/repositories/user_repo.rs:67` | S3 (users admin) |
| ~~`user_repo::find_with_hash_by_id`~~ | ~~`src/repositories/user_repo.rs:67`~~ | **consumed by S3 part 1** (`change_password` reads the stored hash; the method left the dormant list) |
| ~~`MIN_PASSWORD_LEN`~~ | ~~`src/services/identity.rs:38`~~ | **consumed by S3 part 1** (the change form validates through it) |
| ~~`IdentityService::{change_password, revoke_all_sessions}`~~ | ~~`src/services/identity.rs:539`~~ | **consumed by S3 part 1** (the `/password` flow composes them) |

(`src/routes/mod.rs:122`/`:128` — `enforce_credit_limit`, `AppState::new`/
`new_with_credit_limit` — and the finance/products/service items are pre-existing
`main` dead code, not S2 debt.)

**S3 part 1 re-measure (2026-09-20, corrected round):** the first pass measured **68 → 64**;
the correction replaced the revoke-all → prune → re-seat workaround with
`SessionRepository::revoke_all_for_user_except` (one statement, the acting session keeps its id
and expiry), which moved the count to **66**: the four S3 graduations stay
(`change_password` + `MIN_PASSWORD_LEN` + `find_with_hash_by_id` − the re-seat had also made
`revoke_all_for_user`/`revoke_all_sessions` reachable), and the all-or-nothing form returns to
this list awaiting deactivation (S3 part 2) and S4:

| Re-dormant item | Location | Consumed by |
| --- | --- | --- |
| `revoke_all_for_user` | `src/repositories/session_repo.rs:87` | S3 part 2 (deactivation), S4 |
| `revoke_all_sessions` | `src/services/identity.rs:603` | S3 part 2 (deactivation) |

No `#[allow]` attributes were added; the grep above stays empty.

**S3 part 2 re-measure (2026-09-21):** `cargo check --all-targets` **66 → 60 warnings**, no
`#[allow]` attributes; the grep above stays empty. Graduated items (made reachable by the users
administration screen and its service/repo writes):

| Graduated item | Consumed by S3 part 2 |
| --- | --- |
| `Require<P>` construction, `forbidden_response`, `ForbiddenTemplate`, `Principal::has`/`has_permission` | the `/users` routes are the first production handlers to declare and refuse through them |
| `Principal.user_id` (read) | `granted_by` on the grant writes and the reset's actor/target distinction |
| `Role` fields `code`/`name`/`is_system` | the users list and the assignment checkboxes read them |
| `user_repo::find_by_username` | `create_user`'s NOCASE uniqueness pre-check |
| `user_repo::list` (new) | `list_users_with_roles` |
| `role_repo::{find_by_id, list, list_for_user, replace_user_roles}` | the assignment form, the list read and the grant replacement |
| `revoke_all_for_user` + `revoke_all_sessions` | deactivation revokes the deactivated user's sessions |

Still dormant for their slices: `role_repo::{count_active_holders, revoke, delete}` (S4 — `delete`
exists since this slice, message shipped, screen pending), `permission_repo::{list,
codes_for_role->used by assign_roles only}` — actually `codes_for_role` graduated here (the
self-lockout rule reads it); remaining `permission_repo::{list, set_role_permissions, row_to_permission,
map_db_err}` and the `Permission` struct are S4. `Principal.{username, display_name,
must_change_password}` and `Role.{description, created_at, updated_at}` remain S4/S5-S7. The count
must still be back at or below 58 by the end of S7, with no `#[allow]` attributes as the mechanism.

**Requirement:** the count must be back at or below 58 by the end of S7, with **no
`#[allow(dead_code)]` / `#[allow(unused_imports)]` attributes as the mechanism**: each
consuming slice makes its surface reachable (S3 and S4 cover the repositories and
service methods, S5-S7 cover the extractor, the refusal shapes and the principal
reads), and S7's closing check re-runs `cargo check --all-targets`.

### S3 — users administration
- [x] T13: `/users` list with roles and state, create, deactivate/activate, admin password reset, role
      assignment with `granted_by`, and the interface explanation of every trigger refusal.
      (Done by S3 part 2, 2026-09-21: the screen is `routes/users_web.rs` + `templates/users.html` and
      three partials, on the customers pattern; `Require<IdentityUsersRead>` on the reads and
      `Require<IdentityUsersManage>` on the five mutations — the first real consumers of the extractor
      and the full-page refusal page. The service owns the rules: `create_user` (username shape,
      NOCASE uniqueness, `MIN_PASSWORD_LEN`, target flagged), `set_user_active` (deactivation revokes
      all the user's sessions), `admin_reset_password` (flags the target, never the actor; self-reset
      refused), `assign_roles` (records `granted_by`, pre-validates the role ids, refuses the acting
      administrator stripping their own `identity.roles.manage`). The trigger refusals are mapped in
      the repositories to Spanish conflicts: `user_repo::set_active` for the last-protected-holder
      deactivation (AC14), `role_repo::delete` for both the protected role (AC13) and the assigned
      role (AC15). The role-assignment checkboxes repeat the `role_ids` key, which `Form` refuses as a
      duplicate field; that one handler reads the raw form body with a dependency-free parser.)

- [x] T13 (corrected round, 2026-09-21 — the adversarial verification's reproduced administrative
      takeover): the authorization model was split so escalation has nowhere to live. The roles
      endpoint is gated `identity.roles.manage` (the tier that decides who administers the instance;
      the service repeats the tier check so it does not depend on the extractor alone), NOBODY may
      change their own role set with any permission (one rule closes self-escalation and the old
      self-lockout), and the admin password reset applies its tier in the service against the
      TARGET's roles: a protected holder's password additionally requires `identity.roles.manage`,
      so a `identity.users.manage`-only principal cannot take over any administrator. The
      cross-account consequences of each tier are written into the spec ("Cross-account honesty").
      Also in this round: `POST /password` failures share the login's per-username throttle; the
      submitted role ids resolve in one statement (`RoleRepository::find_by_ids`); the oversized
      roles-form body answers `413` in the app's Spanish JSON shape (the raw-body handler owns its
      limit); `role_repo::map_db_err` maps the protected-delete trigger to its Spanish conflict and
      the FK branch is documented as grant-context only; a duplicated `user_id` in the roles form is
      refused instead of silently keeping the last value.
- [x] T13 (second correction round, 2026-09-21 — re-verification: COMMIT WITH NOTED RISK, four
      documentation/UX items): (1) the spec's "Cross-account honesty" no longer promises that
      `identity.roles.manage` alone can reset any password — the reset endpoint is gated
      `identity.users.manage`, so the sentence now states both gates and the both-tiers rule for
      protected holders; (2) the persisted `identity.users.manage` description no longer promises role
      assignment (it is seeded data the S4 matrix renders): the code catalog corrected, migration
      `20240101000029_clarify_identity_permission_descriptions.sql` updates the seeded rows (also the
      underdescribed `identity.roles.manage`, whose gate covers role-set changes), and the AC12 drift
      test now compares DESCRIPTIONS as well as codes (`PERMISSION_DESCRIPTIONS` mirror + a
      one-sided-description mutation test) — the audit of the other 21 descriptions found no further
      contradictions; (3) the "one statement" claim is pinned by a counting role-repository double
      asserting `assign_roles` calls `find_by_ids` exactly once with the whole deduplicated set;
      (4) a present-but-empty `role_ids=` value is the empty set, not a 400 (the UI omits the key;
      the last-holder trigger stays the real backstop), while `%`, `%zz` and non-numeric values stay
      refused.
- [x] T14: `GET`/`POST /password` and the `must_change_password` gate in the middleware.
      (Done by S3 part 1, 2026-09-20, corrected round: the confinement lives in the middleware after
      the session resolution — a full-page request answers `303` to `/password`, `/api/*` a `403` JSON
      reason, an `HX-Request` the refusal with `HX-Redirect: /password`; `/password` (GET/POST), both
      logout endpoints and the public allowlist stay reachable. `POST /password` verifies the current
      password, validates the new one (≥ 12 chars, different, confirmed), updates the hash, clears
      the flag and revokes every other session of the user keeping the acting one, expressed by
      `SessionRepository::revoke_all_for_user_except(user_id, keep_token_hash)` — one UPDATE, no
      window, no re-seat (the first pass's revoke-all → prune → re-insert workaround was removed in
      the correction round). The sidebar gains the password entry; T13's `/users` administration and
      the AC13/AC15 part of T15 are still open.)
- [x] T15: tests for AC13-AC16, AC21 (users surface).
      (AC13/AC14/AC15/AC16 done: S3 part 1 covered AC16 and the confined flow; S3 part 2 ships the
      AC13/AC14 refusals through the screen (last administrator refused with the Spanish reason and
      nothing written, a second administrator created through the screen unblocks), the AC15 message
      in `role_repo::delete`, the reset flagging target-not-actor, the create/activate/deactivate
      round trips, the create-uniqueness conflict and the AC10 shapes on the real routes (403 full
      page / HTMX JSON / write-nothing). AC21 is deferred to S7's nav gating: the sidebar entry is
      shipped visible and the route is what refuses.)

### S4 — roles administration
- [x] T16: `/roles` list, create, edit, delete, and the permission matrix per module and action.
      (Done by S4, 2026-09-22: the screen is `routes/roles_web.rs` + `templates/roles.html` and
      two partials, on the users-screen pattern — create `<dialog>`, id-final edit fragment
      carrying the details form AND the matrix, `HX-Trigger` events `role-created`/`role-changed`,
      collection endpoints with the id in the body, and the raw-body parse pattern with its own
      64 KiB limit for the matrix form whose checkboxes repeat `permission_ids`. Service rules:
      `list_roles_with_holders` (holders of any state: the RESTRICT FK does not distinguish),
      `create_role` (code shape and uniqueness, name, optional description ≤ 256), `update_role`
      (name/description only — the code is never an editable field), `delete_role` (refuses a held
      role NAMING the blocking users, refuses the protected role), `role_matrix` (the 23-row
      catalog with the held ids), `set_role_matrix` (deduplicates and resolves the submitted set
      in ONE `find_by_ids` statement — pinned by a counting double — refuses the protected role's
      matrix, and refuses the NEW self-lockout rule: an edit removing `identity.roles.manage` from
      a role the acting principal holds). Every mutation repeats the `identity.roles.manage` tier
      check in the service, as `assign_roles` does. Gating: every surface — reads included —
      declares `Require<IdentityRolesManage>` (deliberate, no `identity.roles.read` exists and a
      new code would need a migration; the cost is written into the spec). The sidebar gains the
      Roles entry. The two trigger mappings the ledger left pending are closed: the protected
      rename (`protected role code cannot change`) and the protected matrix removal
      (`protected role permissions cannot be removed`) map to their own Spanish conflicts in the
      repositories, plus the schema CHECK refusals for the role's fields.)
- [x] T17: tests for AC17, AC13 (through the interface), AC15.
      (Done by S4: AC17 — a matrix edit through the screen applies to the next request, proven
      both ways by ticking `identity.users.read` onto the actor's role and watching `GET /users`
      go 403 → 200 → 403 without a restart; AC13 — the protected role's matrix edit and delete
      refused with the Spanish reason and nothing written, the dialog rendering read-only, no
      delete offered, no code field for any role; AC15 — the held role's deletion refused naming
      the blocking users; plus the self-lockout refusal, the under-permissioned principal refused
      on the page and on every mutation in the right shape with nothing written, the create/edit
      round trips with the duplicate-code conflict, the malformed-code and malformed-id refusals,
      the oversized matrix body answering 413 in the app shape, the duplicated `role_id` refused,
      and the whole-set replacement semantics. Service level: the Spanish refusals before any
      write and the counting double pinning the one-statement resolution.)

#### S4 correction round (2026-09-22 — verification: COMMIT WITH NOTED RISK, one documentation
finding + three claims with no test behind them; text plus tests, no behaviour change)
1. **MAJOR (documentation): the spec overclaimed the self-escalation closure.** The no-self-role
   change sentence claimed one rule closes "self-escalation and the self-lockout alike", but it
   only closes the DIRECT path; the matrix reopens the indirect one (a roles-manage holder ticks
   `identity.users.manage` onto a role it holds, gets `200`, and holds the pair on the next
   request — AC17 codifies self-granting as intended). Code right, text wrong: the sentence is
   rescoped to changing your own role set directly, and Cross-account honesty now states the
   consequence plainly, in the operator's words — with `identity.roles.manage` the holder can add
   ANY permission to a role it occupies, the pair can reset a protected holder's password, and the
   permission hands over the instance, not merely the decision of who administers. The rest of the
   block re-read: no other claim is contradicted by the matrix path.
2. **MINOR: the service-level protected-matrix refusal was not test-pinned** — disabling the
   `is_system` check in `set_role_matrix` left `ac13_..._locked_through_the_screen` green (the
   SQLite trigger plus its mapping satisfy it). The guard STAYS — it is not a duplicate of the
   backstop: the trigger only blocks the REMOVAL half of the replacement, while the service
   refuses the whole edit class before any statement reaches the database, with its own message
   (the sentence AC13 pins on the screen) and its own error precedence (it refuses before the
   form validation could answer a missing permission id). New service test pins the service's own
   conflict against the empty set, a real id, and a non-existent id; mutation-validated (guard
   disabled → the new test fails, the screen test stays green).
3. **MINOR: "a deactivated holder is named" was claimed and untested.** `holder_names` has no
   `is_active` filter on purpose; a repository test now pins the TOTAL set (a deactivated holder
   is named too) and the still-refused deletion; mutation-validated (adding the
   `is_active = 1` filter → the test fails).
4. **NIT: the `users.manage`-only gate case was untested** — the gate tests used an
   empty-permission principal, which cannot separate "no permission" from "the wrong permission".
   New route test: a principal holding `identity.users.read` + `identity.users.manage` but not
   `identity.roles.manage` is refused the roles page (403 HTML naming the gate) and the matrix
   mutation (403 JSON naming the gate), reaches `/users` (fixture proof), and writes nothing;
   mutation-validated (page gate swapped to `IdentityUsersManage` → the new test fails).
- Numbers: `cargo test` 541 → **544 passed / 0 failed** (+3 tests, no behaviour change);
  `cargo check --all-targets` **0 errors, 56 warnings** (unchanged); allows grep empty.

#### S4 warning ledger re-measure (2026-09-22)
`cargo check --all-targets` **60 → 56 warnings**, no `#[allow]` attributes; the grep stays empty.
Graduated items (made reachable by the roles screen and its service/repo writes):

| Graduated item | Consumed by S4 |
| --- | --- |
| `permission_repo::{list, set_role_permissions, row_to_permission, map_db_err}` | the matrix read (`role_matrix`) and its replacement (`set_role_permissions`, refusals mapped) |
| `Permission` model struct (as a constructed, read value) | the matrix rows; residue: fields `action`/`created_at` are still never read (the matrix renders code + description) |
| `role_repo::delete` | the delete flow through the service (the mapped refusals the S3 round shipped are now screen-backed) |
| `Role.description` | the list rows and the edit dialog |

New surface written and consumed at birth (no new warnings): `role_repo::{create,
update_details, holder_names}`, `permission_repo::find_by_ids`, the service methods
(`list_roles_with_holders`, `create_role`, `update_role`, `delete_role`, `role_matrix`,
`set_role_matrix`), the models (`NewRole`, `RoleWithHolders`, `RoleMatrix`) and the routes with
their templates. Still dormant for their slices: `role_repo::{count_active_holders, revoke}`
(S4's deletion blocks on ALL holders — the total set via `holder_names` — and grant/revocation
lives on the users screen, so their natural consumer is a future holder-view flow; noted for
S7's re-measure), `Role.{created_at, updated_at}`, `Permission.{action, created_at}` and
`Principal.{username, display_name, must_change_password}` (S5-S7). The count must still be
back at or below 58 by the end of S7, with no `#[allow]` attributes as the mechanism.

### S5 — enforcement: finance and inventory
- [ ] T18: `Require<P>` per action on every route of both departaments, nav gating, 403 fragment for HTMX.
- [ ] T19: tests for AC10 on the real handlers, AC21, and the exposure guard over the two departaments.

### S6 — enforcement: sales and customers
- [ ] T20: `Require<P>` per action, nav gating, and the refusal fragment on the drawer/modal flows.
- [ ] T21: tests for AC10 and AC21 on the real handlers.

### S7 — enforcement: purchases, suppliers, identity, dashboard
- [ ] T22: `Require<P>` per action, nav gating, dashboard and identity screens gated.
- [ ] T23: tests for AC10 and AC21, and the full-surface exposure guard.

### S8 — Phase A close
- [ ] T24: browser slice (`e2e/tests/test_identity.py`) for AC22, wired into the harness login step.
- [ ] T25: README (no-auth section, module table, migrations, environment), `env.example`, and
      `openspec/specs/identity/spec.md` promoted; the change folder archived for Phase A.

## Phase B — audit
- [ ] T26 (S9): audit migration for finance tables, actor plumbing, display, tests for AC18-AC19.
- [ ] T27 (S10): audit for inventory tables.
- [ ] T28 (S11): audit for sales and customer receipts.
- [ ] T29 (S12): audit for purchases, suppliers and supplier costs.
- [ ] T30 (S13): audit for the identity tables themselves (roles, permissions) and the grant trail.
- [ ] T31 (S14): closing verification, `openspec/specs/identity/spec.md` audit section, archive.

## Verify
- [ ] `cargo test` green at the end of every slice, with the new tests present and the regression tests
      validated by reintroducing the bug in a throwaway copy.
- [ ] Every test that claims a guard must be mutation-validated: S1a shipped two tests whose names asserted
      properties they could not detect (`ac5_success_clears_the_counter`, the prune revoked branch), both
      caught by independent verification and fixed. No slice closes without its own mutation table.
- [ ] AC25 holds for every new comparison against a database-written timestamp.
- [ ] `cargo check --all-targets` with no new errors.
- [ ] Browser suite green for the identity slice, plus a manual smoke per enforcement slice: log in as a
      restricted user, confirm the refused action is refused at the handler.
- [ ] Independent verification of each slice before its PR, and a post-merge gate after Phase A.

## Archiving
- [ ] On merge of Phase A: create `openspec/specs/identity/spec.md` from AC1-AC17 and AC20-AC23, update the
      `verification` capability with the new browser cases, and move this change's Phase A scope to
      `openspec/changes/archive/`.
- [ ] On merge of Phase B: extend `openspec/specs/identity/spec.md` with the audit rules and AC18-AC19, and
      archive the remaining scope.
