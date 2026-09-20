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
| `role_repo::{find_by_id, list, list_for_user, replace_user_roles}` | `src/repositories/role_repo.rs:54` | S3 (assignment form, deactivation), S4 (roles admin) |
| `role_repo::{count_active_holders, revoke}` | `src/repositories/role_repo.rs:54` | **no known consumer yet** (corrected by the S7 part 2 correction round: the original S2 row assigned them to S3/S4 and the closing ledgers carried them to Phase B, but neither slice consumed them and Phase B's audit columns will NOT — `revoke` is the single-grant removal that surfaces the AC14 guard and `count_active_holders` the AC15 holder count, both shipped as the S2 trait surface exercised by tests; production grant/revocation lives on the users screen through `replace_user_roles` and deletion blocks on `holder_names` plus the RESTRICT FK) |
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
must still be back at or below 56 by the end of S7 (the running target has been 56 all along —
`main` measures 56 with the summary-lines-excluded count; the 58 that circulated in the S2 era
included the two per-target summary lines), with no `#[allow]` attributes as the mechanism.

**Requirement:** the count must be back at or below 56 by the end of S7, with **no
`#[allow(dead_code)]` / `#[allow(unused_imports)]` attributes as the mechanism**: each, with **no
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
back at or below 56 by the end of S7, with no `#[allow]` attributes as the mechanism.

### S5 — enforcement: finance and inventory (T18–T19) — ENTREGADA 2026-09-19, con el nav gating deferido a S7
- [x] T18: `Require<P>` per action on every route of both departaments (per-handler everywhere; the
      drawer fragment carries a double gate) — **nav gating is NOT in this slice: it stays S7's**, as
      the S5 brief fixes ("Do not gate the navigation: hiding sidebar entries needs the principal
      plumbed into every page struct, which is S7's slice"). The 403 shapes already existed; this
      slice's handlers are their first finance/inventory consumers.
- [x] T19: tests for AC10 on the real handlers (16 new tests, mutation-validated) — AC21's exposure
      guard at the HANDLER level is done for both departments; the interface-hiding half of AC21 is
      S7. The full-surface exposure guard grep belongs to S7's T23.

#### S5 warning ledger re-measure (2026-09-19, CORRECTED in the verification round)
Final measurement, in the slice's end state, counting only lint warnings (the two per-target
`generated N warnings` summary lines are not lint warnings and must not be counted):
**56 warnings, delta 0 — the brief's number and the S4 note were both correct.** The writer's
first pass recorded 58 on both the branch and a throwaway HEAD copy because the count script
included the two summary lines; the re-measure method (raw `warning:` lines minus summaries)
settles the running record back at **56 both sides**. No `#[allow]` attributes; the grep stays
empty. NO dormant ledger item graduates by warning count in this slice: the items the S2 table
attributed to S5 (`Require<P>` construction, `forbidden_response`, `ForbiddenTemplate`,
`Principal::has`/`has_permission`) had already graduated with S3 part 2's `/users` routes; S5
widens their production consumers (30+ finance/inventory handlers) without moving the count.
Still dormant: `Principal.{username, display_name, must_change_password}` (S7 navigation),
`role_repo::{count_active_holders, revoke}`, `Role.{created_at, updated_at}`,
`Permission.{action, created_at}`. The count must still be back at or below 56 by the end of S7,
with no `#[allow]` attributes as the mechanism. (The earlier "≤ 58" target in this section and
the D2/S2 requirement text carry the same stale number — S7's re-measure must use 56, which is
what `main` has always measured.)

#### S5 enforcement mapping (written by the writer round of 2026-09-19)
Per-handler `Require<P>` on every finance and inventory route; NO router-level guard is used — each
module mixes read/write/stock-write on the same paths, so a module-level layer would over-gate the
reads. One handler (the product drawer fragment) declares TWO extractors because it renders two
owners' data (see the note below). Purchases/sales/customers/suppliers routes are untouched (S6/S7).

`src/routes/api.rs` (finance JSON):

| Route | Method | Permission |
| --- | --- | --- |
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

`src/routes/web.rs` (dashboard + finance HTML/HTMX):

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

`src/routes/inventory_api.rs`:

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

`src/routes/inventory_web.rs` (products screen):

| Route | Method | Permission |
| --- | --- | --- |
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

Mapping decisions worth the reviewer's attention:
- Account creation (`POST /api/accounts`, `POST /web/accounts`) is gated `finance.methods.manage`, not
  `finance.write`: the seeded description of that code is "Administrar cuentas y medios de pago" and
  `finance.write`'s is "Registrar y editar movimientos" — the ledger structure is the manage tier.
- `GET /api/payment-methods` stays `finance.read` (a read of the methods catalog); the ALLOWLIST
  endpoints (both directions of `/api/accounts/{id}/payment-methods` and the web save) are the
  `finance.methods.manage` surface.
- Known consequence, recorded deliberately (fail-closed, not a hole): a principal holding ONLY
  `finance.methods.manage` is refused `GET /api/payment-methods` — the catalog of the very
  payment methods it may administer. No seeded matrix separates the codes (whoever holds
  `methods.manage` also holds `finance.read` in every catalog matrix, and the protected role
  holds everything), so no operator hits it today. If a future matrix separates them, the
  correct shape is an OR of the two codes on that one read — do NOT invent a new kernel type for
  it; the single-code annotation stays the v1 contract.
- The account-detail page is a finance READ (`finance.read`): a read-only operator can view the
  account and its transactions; the allowlist form it renders is refused server-side on submit. The
  interface hiding is S7 (AC21), the handler is the enforcement.
- `/web/products/detail/{id}` (the product drawer) requires `inventory.read` AND
  `purchases.costs.read`: the fragment renders per-supplier cost rows, so an inventory-only principal
  must get the refusal instead of cost data it may not see (the seeded `deposito` role holds both).
- `POST /web/product-costs{,/preferred}` carry `purchases.costs.write` — the codes of the module that
  owns the data, as the S5 brief fixes, even though the form lives in the product drawer.
- Residual noted honestly: a principal holding `purchases.costs.write` WITHOUT `inventory.read` that
  records a cost through the non-drawer branch receives the catalogue list fragment (the issue #37
  filter answer), which is inventory data. No natural operator holds that combination (the catalog's
  seeded matrices never separate them), so the slice keeps the single-code annotation and documents
  the edge instead of over-gating.

#### S5 fixture resolution and final numbers (2026-09-19, correction round — Option 1 approved)
The first pass seeded the shared test principal with all 23 codes, which made the identity screens'
refusal fixtures (users/roles) red. Two variants were measured; the user approved the one
implemented: the shared principal holds a CUSTOM role (`probe_all`) with all 23 catalog codes
through the same real grant path the bootstrap performs (`roles.grant`, `granted_by` = the user
itself, idempotent) — NOT the protected `admin` role, because a second seeded protected holder
would make `bootstrap_admin` see an existing holder and never create the administrator, breaking
the login fixtures' semantics (measured: 20 failures across 4 files under that variant vs. 11
under this one). The authorized fixture-wiring follow-up: `users_web.rs` and `roles_web.rs`
`test_pool()` switched to `seed_session_without_roles` (fixture wiring ONLY — no assertion, status
or body check touched, no test renamed). The permissionless variant is exactly what those screens'
tests built on since S3/S4; the happy-path tests keep granting their own sets through
`app_with_permissions`.

Final S5 numbers: `cargo test` 544 → **560 passed / 0 failed** (+16 enforcement tests; the 11
fixture-premise tests returned green with wiring only); `cargo check --all-targets` 0 errors,
**58 warnings (delta 0 vs HEAD measured the same day)**; the dead-code/unused-import allow grep is empty — the two `#[allow` hits a repo-wide grep finds in `src/` predate this feature and suppress nothing here: a `clippy::too_many_arguments` on a smoke-suite helper and a mention inside a doc comment;
`scripts/e2e.sh -k identity` 4 passed, **-k products 13 passed / 1 skipped (opt-in screenshot
probe)**, `-k filters` 7 passed. Live probe with the real binary (throwaway DB,
`ROYA_ADMIN_PASSWORD` set): admin login → `GET /` 200, `GET /products` 200, `GET /web/accounts`
200, `POST /web/accounts` 303, `POST /api/transactions` 201; negative case end-to-end — a real
user holding only `vendedor` (created and assigned through the screens, confinement lifted via
`/password`): `GET /products` 200 (holds `inventory.read`), `POST /web/accounts` 403 HTML naming
`finance.methods.manage`, `POST /api/products` 403 JSON naming `inventory.write`, HTMX
`POST /web/products` 403, `PUT /api/accounts/1/payment-methods` 403, `GET /api/accounts` 403 —
and the administrator still answers 200.

#### S5 correction round (2026-09-19, verification: COMMIT WITH NOTED RISK — one MAJOR + two NITs)

1. **MAJOR (coverage): two gates had no mutation-visible test.** Removing `Require<InventoryWrite>`
   from `web_edit_product` and `Require<DashboardRead>` from the dashboard left the whole suite
   green — the first round's AC10 tests only reached `/web/products` POST, `/web/stock-movements`
   and `/web/product-costs`. Fixed with permission-refusal tests for every handler the first round
   did not pin, each in the shape its caller reads. All annotations kept as annotated (each
   handler's action really is the permission it declares); none needed re-mapping.

   New tests (6, in `inventory_web.rs` and `web.rs`):
   - `the_product_edit_gate_refuses_an_inventory_read_only_principal` — HTMX JSON naming
     `inventory.write`; also proves the refused edit left the product's stored name untouched.
   - `the_product_activate_gate_refuses_an_inventory_read_only_principal` — HTMX JSON.
   - `the_product_deactivate_gate_refuses_an_inventory_read_only_principal` — HTMX JSON; also
     asserts `is_active` did not flip.
   - `the_product_delete_gate_refuses_an_inventory_read_only_principal_and_writes_nothing` —
     plain browser post → full-page HTML refusal naming `inventory.write`; counts `products`
     rows before/after (no-write proof).
   - `the_category_gate_refuses_an_inventory_read_only_principal` — HTMX JSON; counts
     `categories` rows before/after (no-write proof).
   - `the_dashboard_gate_refuses_a_principal_without_it_and_opens_with_it` — an inventory-only
     principal gets the full-page refusal naming `dashboard.read`; a `dashboard.read`-holding
     principal gets 200.

   The movement-creation gate was already pinned by the first round's
   `ac10_an_inventory_read_only_principal_is_refused_the_htmx_mutations` (an inventory.read-only
   principal is refused there); this round's mutation table proves that too.

   **The round's mutation table** (each gate removed, its test observed FAILING, then restored;
   the round's final diff is tests-only — no annotation survived removed):

   | # | Annotation removed | Test that failed | Observed failure |
   | --- | --- | --- | --- |
   | M1 | `web_edit_product`: `Require<InventoryWrite>` | `the_product_edit_gate_refuses_an_inventory_read_only_principal` | 200 with the list fragment (the edit ran) instead of 403 |
   | M2 | `web_activate_product`: `Require<InventoryWrite>` | `the_product_activate_gate_refuses_an_inventory_read_only_principal` | 200 with the list fragment instead of 403 |
   | M3 | `web_deactivate_product`: `Require<InventoryWrite>` | `the_product_deactivate_gate_refuses_an_inventory_read_only_principal` | 200 with the list fragment instead of 403 |
   | M4 | `web_delete_product`: `Require<InventoryWrite>` | `the_product_delete_gate_..._writes_nothing` | **303** (the delete really happened and redirected) instead of 403 |
   | M5 | `web_create_category`: `Require<InventoryWrite>` | `the_category_gate_refuses_an_inventory_read_only_principal` | 200 instead of 403 |
   | M6 | `web_create_movement`: `Require<InventoryStockWrite>` | `ac10_an_inventory_read_only_principal_is_refused_the_htmx_mutations` (first-round test) | **404 `product 1 not found`** — the handler ran instead of the gate refusing |
   | M7 | `dashboard`: `Require<DashboardRead>` | `the_dashboard_gate_refuses_a_principal_without_it_and_opens_with_it` | 200 instead of 403 |

   No test failed to bite; none needed a forced justification, and no handler turned out to
   declare the wrong permission (edit/lifecycle/delete/category are product mutations =
   `inventory.write`; the dashboard reads `dashboard.read`).

2. **NIT (ledger count): the writer's first-pass method counted the two per-target `generated N
   warnings` summary lines, reporting 58. The real count — raw `warning:` lint lines minus
   summaries — is 56 in both the branch and `main`; the brief and the S4 note were correct all
   along. The S5 warning-ledger section was rewritten with the corrected method and number, and
   every "≤ 58" target in this file and the ODD ledger now reads ≤ 56.**

3. **NIT (half-documented over-gating): a principal holding ONLY `finance.methods.manage` is
   refused `GET /api/payment-methods` — the catalog of the methods it may administer.** Recorded
   in the mapping decisions as a known fail-closed consequence: no seeded matrix separates the
   codes today, and if a future matrix does, the fix is an OR of the two codes on that one read,
   not a new kernel type.

### S6 — enforcement: sales and customers — ENTREGADA (writer round) 2026-09-20, nav gating deferido a S7
- [x] T20: `Require<P>` per action on every route of both departments (52 handler gates over 41 route
      paths: 11 sales API + 12 customers API + 17 sales web + 12 customers web) — **nav gating is NOT in
      this slice: it stays S7's**, as the S5/S6 briefs fix. The refusal fragments need no new work: the
      HTMX JSON shape and the full-page refusal card already answer every gated drawer/modal endpoint.
- [x] T21: tests for AC10 on the real handlers (18 new tests, every one of the 52 gates
      mutation-validated one gate at a time) — AC21's exposure guard at the HANDLER level is done for
      both departments; the interface-hiding half is S7, as in S5.

#### S6 enforcement mapping (written by the writer round of 2026-09-20)
Per-handler `Require<P>` on every route; NO router-level guard (same reason as S5: the modules mix
read/write/cancel on the same paths). The collection adapters (`/web/sales/{lines,confirm,payments,cancel}`)
and the path-based handlers they mirror were refactored to share ungated `*_impl` bodies so BOTH
registered handlers declare their own real gate — see the mutation-table note below.

`src/routes/sales_api.rs`:

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

`src/routes/customers_api.rs`:

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

`src/routes/sales_web.rs`:

| Route | Method | Permission |
| --- | --- | --- |
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

`src/routes/customers_web.rs`:

| Route | Method | Permission |
| --- | --- | --- |
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

Mapping decisions worth the reviewer's attention (the four judgement calls the S6 brief named):
- **A payment on a sale is `customers.collect`, not `sales.create`** (the drawer form, its JSON twin and
  the collection adapter): the code names the capability, and money received against an owed balance is
  a collection — the `sale_payments` row posts its own Income transaction, exactly what the
  customer-receipt collect does per allocation. The cost, recorded deliberately (fail-closed): a
  principal holding `sales.create` WITHOUT `customers.collect` cannot register a payment on the sale it
  recorded. No seeded matrix separates the pair (both `vendedor` and `cajero` hold both codes; the
  protected role holds everything), so no natural operator is hit. The S5 contract holds: if a future
  matrix separates them, the fix is an OR of the two codes on that one action, not a new kernel type.
- **The customer-receipt flow (`/api/customer-receipts`, `/web/customer-receipts`) is
  `customers.collect` for the collect and `customers.read` for its reads**: grouping several invoices
  into one receipt is the capability the code was written for. Receipt reads are reads of the
  customer's account (`customers.read`) — the same gate the statement uses — so a read-only operator
  can see the collections it may not create.
- **The customer statement (`/api/customers/{id}/statement`, `/customers/{id}`, the drawer detail
  fragments) is a single `customers.read` gate**, not an AND with `sales.read`: the statement renders
  THAT customer's own documents as the receivable ledger, which is the customer module's own view; the
  cost is stated plainly — a `customers.read`-only principal sees the sale documents of that customer
  (number/date/total), data the receivable is meaningless without — but never the sales list or another
  customer's sales. Same shape as S5's deliberate consequences; no seeded matrix separates the pair.
- **The sale-debt page (`/web/sales/debt`, `/api/sales/debt`) is `sales.read`**: the debt summary is
  unpaid SALES (the sales screen's banner), not customer data. The mirrored cost: a
  `customers.read`-only collector is refused the sales debt report; `cajero` holds `sales.read`, so no
  seeded operator is hit.
- **Confirming a cash sale stays `sales.create` even though it embeds a tender**: the confirm-embedded
  payment is part of the sale lifecycle itself (one Income + payment row in the same step); the
  standalone payment endpoints are where `customers.collect` draws the line.
- **Cross-capability dependency (fail-closed, recorded):** creating a sale needs a customer — the
  create dialog's selector is rendered server-side into `/sales` (no extra fetch), but the record
  page's product picker reads `/web/product-search` (`inventory.read`, S5-gated) and an under-permissioned
  client cannot resolve products it may not see. Seeded sales holders (`vendedor`, `cajero`) hold
  `inventory.read`, so no natural operator is hit; the server-side `resolve_product_ref` inside
  `web_add_line` stays a service composition, not a permission grant. Single-code annotations remain
  the v1 contract; an OR is the documented future shape, per S5.

#### S6 mutation table (writer round — every gate removed, its test observed FAILING, then restored)

The S5 lesson was enforced from the first pass, and it caught a real gap early: the first test wave used
a read-only probe (holding `sales.read`/`customers.read`) whose READ assertions (expect 200) stay green
when the read gate is deleted — the exact S5 "no test would notice the removal" class. Caught by the
first mutation run (3 of 11 sales_api gates survived deletion), fixed by adding a
`the_read_gates_refuse_a_principal_without_the_read_permission` test to EACH of the four modules (a
probe holding an UNRELATED permission — never the empty-set probe — so a wrong-permission gate cannot
pass for a broken fixture). After the fix, all 52 gates bite.

Mutation method: one gate at a time (deleting a whole cluster would only bite at the first refusal of
each test), test observed failing, gate restored; the final diff keeps every annotation. The four
sales_web collection adapters initially delegated with a manually constructed `Require::default()`,
which would have made the inner handlers' gates removal-safe only by compile error; the adapters and
path handlers now share ungated `*_impl` bodies and each registered handler declares its own gate, so
every gate fails a test at runtime instead of breaking the build.

| Cluster | Gates | Test(s) that fail when the gate is removed |
| --- | --- | --- |
| sales API reads (list, debt, detail) | 3 × `SalesRead` | `the_read_gates_refuse_a_principal_without_the_read_permission` (403→200) |
| sales API draft lifecycle (create, update, add/update/remove line, confirm) | 7 × `SalesCreate` | `ac10_a_sales_read_only_principal_...` (403→201/200) and, for create, `ac10_a_sales_refusal_writes_nothing` (row count grows) |
| sales API payment | `CustomersCollect` | both AC10 refusal tests (403→201; payment row count grows) |
| sales API cancel | `SalesCancel` | both AC10 tests (403→200; status flips to Cancelled) |
| customers API reads (list, get, ageing, statement, receipts reads) | 6 × `CustomersRead` | `the_read_gates_refuse_a_principal_without_the_read_permission` |
| customers API entity writes (create, update, delete, activate, deactivate) | 5 × `CustomersWrite` | `ac10_a_customers_read_only_principal_...` and, for create/delete, `ac10_a_customers_refusal_writes_nothing` |
| customers API collect | `CustomersCollect` | both AC10 tests (receipt count grows) |
| sales web reads (page, record page, list, debt, detail fragments) | 5 × `SalesRead` | `the_read_gates_refuse_a_principal_without_the_read_permission` |
| sales web draft lifecycle (create, add/update/remove line, confirm, header) | 6 × `SalesCreate` | `ac10_a_sales_read_only_principal_...` and, for create/confirm, `ac10_the_sales_web_refusal_writes_nothing` |
| sales web payment (path + collection adapter) | 2 × `CustomersCollect` | the AC10 pair / `the_sales_collection_adapters_carry_their_own_gate` |
| sales web cancel (path + collection adapter) | 2 × `SalesCancel` | the AC10 pair / `the_sales_collection_adapters_carry_their_own_gate` |
| sales web collection adapters (lines, confirm) | 2 × `SalesCreate` | `the_sales_collection_adapters_carry_their_own_gate` |
| customers web reads (page, statement page, list, detail, edit form, receipts fragments) | 6 × `CustomersRead` | `the_read_gates_refuse_a_principal_without_the_read_permission` |
| customers web entity writes (create, edit, activate, deactivate, delete) | 5 × `CustomersWrite` | `ac10_a_customers_read_only_principal_...` and, for delete, `ac10_the_customers_web_refusal_writes_nothing` |
| customers web collect | `CustomersCollect` | both AC10 tests (receipt count grows) |

52 gates removed individually → 52 observed failures; no gate needed a forced justification and no
handler turned out to declare the wrong permission (the four judgement calls above survived the
mutation review unchanged).

New tests (18, named per module): `ac10_a_sales_read_only_principal_reads_and_is_refused_the_writes`,
`ac10_a_sales_refusal_writes_nothing`, `ac10_the_sales_holding_principal_gets_the_normal_answer`,
`the_read_gates_refuse_a_principal_without_the_read_permission`,
`an_anonymous_request_still_gets_the_json_gate_not_the_permission_refusal` (sales_api); the same
five-name pattern for customers_api; `ac10_a_sales_read_only_principal_is_refused_the_web_mutations_in_both_shapes`,
`ac10_the_sales_web_refusal_writes_nothing`, `ac10_the_sales_web_holding_principal_gets_the_normal_answer`,
`the_sales_collection_adapters_carry_their_own_gate`, plus the read-gate and anonymous tests (sales_web);
`ac10_a_customers_read_only_principal_is_refused_the_web_mutations`,
`ac10_the_customers_web_refusal_writes_nothing`,
`ac10_the_customers_web_holding_principal_gets_the_normal_answer`, plus the read-gate and anonymous
tests (customers_web).

#### S6 warning ledger re-measure (2026-09-20, end state)
`cargo check --all-targets` **0 errors, 56 lint warnings (delta 0 vs `main`, measured the ledger way:
58 raw `warning:` lines minus the 2 per-target summary lines)**. No `#[allow]` attributes added; the dead-code/unused-import allow grep is empty — the two `#[allow` hits a repo-wide grep finds in `src/` predate this feature and suppress nothing here: a `clippy::too_many_arguments` on a smoke-suite helper and a mention inside a doc comment. NO dormant ledger item graduates by warning count in this slice — `Require<P>`
construction, the refusal shapes and `Principal::has`/`has_permission` had already graduated with S3
part 2, and this slice only widens their production consumers (52 more handlers). Still dormant:
`Principal.{username, display_name, must_change_password}` (S7 navigation),
`role_repo::{count_active_holders, revoke}`, `Role.{created_at, updated_at}`,
`Permission.{action, created_at}`. The count must still be back at or below 56 by the end of S7, with
no `#[allow]` attributes as the mechanism. `test_support.rs` needed NO change this slice:
`seed_session_with_permissions` covered every probe the tests needed.

### S7 part 1 — enforcement: purchases and suppliers (T22 in part) — ENTREGADA (writer round) 2026-09-20, nav gating deferido a S7 part 2
- [x] T22 (part): `Require<P>` per action on every route of both departments (51 handler gates over 40
      route paths: 21 purchases/suppliers API handlers + 18 purchases web handlers + 12 suppliers web
      handlers) — **nav gating is NOT in this slice: it stays S7 part 2's**, as the S5/S6/S7 briefs
      fix. The refusal fragments needed no new work: the HTMX JSON shape and the full-page refusal
      card already answer every gated page, fragment and drawer endpoint.
- [x] T23 (part): tests for AC10 on the real handlers (17 new tests, every one of the 51 gates
      mutation-validated one gate at a time) — AC21's exposure guard at the HANDLER level is done for
      both departments; the interface-hiding half (and the full-surface exposure guard grep) is S7
      part 2, as in S5/S6.

#### S7 part 1 enforcement mapping (written by the writer round of 2026-09-20)
Per-handler `Require<P>` on every route; NO router-level guard (same reason as S5/S6: the modules mix
read/write/cancel on the same paths). The four purchases_web collection adapters
(`/web/purchases/{lines,confirm,payments,cancel}`) were refactored to share ungated `*_impl` bodies so
BOTH registered handlers declare their own real gate — the same S6 refactor, every gate fails a test at
runtime instead of breaking the build.

`src/routes/purchases_api.rs` (purchases + the suppliers/cost REST twins):

| Route | Method | Permission |
| --- | --- | --- |
| `/api/purchases` | GET | `purchases.read` |
| `/api/purchases/suggestions` | GET | `inventory.read` (judgement C) |
| `/api/purchases` | POST | `purchases.create` |
| `/api/purchases/{id}` | GET | `purchases.read` |
| `/api/purchases/{id}` | PUT | `purchases.create` |
| `/api/purchases/{id}/lines` | POST | `purchases.create` |
| `/api/purchases/lines/{line_id}` | PUT | `purchases.create` |
| `/api/purchases/lines/{line_id}` | DELETE | `purchases.create` |
| `/api/purchases/{id}/payments` | POST | `purchases.create` (judgement A) |
| `/api/supplier-payments` | POST | `purchases.create` (judgement A) |
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

`src/routes/purchases_web.rs` (purchases screen):

| Route | Method | Permission |
| --- | --- | --- |
| `/purchases` | GET | `purchases.read` |
| `/purchases/{id}` | GET | `purchases.read` |
| `/web/purchases` | GET | `purchases.read` |
| `/web/purchases` | POST | `purchases.create` |
| `/web/purchases/suggestions` | GET | `inventory.read` (judgement C) |
| `/web/purchases/from-suggestion` | POST | `purchases.create` (judgement D) |
| `/web/purchases/lines` | POST | `purchases.create` (adapter) |
| `/web/purchases/confirm` | POST | `purchases.create` (adapter) |
| `/web/purchases/payments` | POST | `purchases.create` (adapter, judgement A) |
| `/web/purchases/cancel` | POST | `purchases.cancel` (adapter) |
| `/web/purchases/{id}` | GET | `purchases.read` |
| `/web/purchases/{id}/lines` | POST | `purchases.create` |
| `/web/purchases/{purchase_id}/lines/{line_id}` | POST | `purchases.create` |
| `/web/purchases/{purchase_id}/lines/{line_id}` | DELETE | `purchases.create` |
| `/web/purchases/{id}/header` | POST | `purchases.create` |
| `/web/purchases/{id}/confirm` | POST | `purchases.create` |
| `/web/purchases/{id}/payments` | POST | `purchases.create` (judgement A) |
| `/web/purchases/{id}/cancel` | POST | `purchases.cancel` |

`src/routes/suppliers_web.rs` (suppliers screen):

| Route | Method | Permission |
| --- | --- | --- |
| `/suppliers` | GET | `suppliers.read` |
| `/web/suppliers` | GET | `suppliers.read` |
| `/web/suppliers` | POST | `suppliers.write` |
| `/web/suppliers/edit` | POST | `suppliers.write` |
| `/web/suppliers/{id}` | DELETE | `suppliers.write` |
| `/web/suppliers/{id}/detail` | GET | `suppliers.read` AND `purchases.costs.read` (two extractors, judgement B) |
| `/web/suppliers/{id}/edit-form` | GET | `suppliers.read` |
| `/web/suppliers/{id}/activate` | POST | `suppliers.write` |
| `/web/suppliers/{id}/deactivate` | POST | `suppliers.write` |
| `/web/supplier-costs` | POST | `purchases.costs.write` |
| `/web/supplier-payments` | POST | `purchases.create` (judgement A) |

Mapping decisions worth the reviewer's attention (the four judgement calls the S7 brief named):
- **A payment to a supplier is `purchases.create`, not `suppliers.write`** (both payment endpoints:
  the per-purchase record and the supplier-level handover): the catalog has no `purchases.pay` and the
  payment is a PURCHASE-side movement — it writes `purchase_payments` rows against purchases, posts the
  Expense through the finance kernel, and lives in `PurchasesService` (`pay_supplier`), exactly where
  `customers.collect` lives in the sales mirror. `suppliers.write` is the supplier ENTITY tier (create,
  edit, deactivate, delete a supplier), and granting entity editing must not hand over money
  movements. The cost, recorded deliberately (fail-closed): a principal holding `suppliers.write`
  WITHOUT `purchases.create` sees the pay card in the drawer it may open (its read gates hold) and is
  refused on submit, naming `purchases.create`. No seeded matrix separates the pair (no seeded role
  holds `suppliers.write` at all, and the protected role holds everything), so no natural operator is
  hit; if a future matrix separates them, the fix is an OR of the two codes on that one action, not a
  new kernel type (the S5/S6 contract).
- **B: the supplier drawer (`/web/suppliers/{id}/detail`) is a DOUBLE gate — `suppliers.read` AND
  `purchases.costs.read`** — because the fragment renders per-supplier cost rows, the SAME data the S5
  product drawer already gates `purchases.costs.read`; one dataset reached from two screens must not
  answer to two permissions. The costs write keeps the data owner's code too: `POST /web/supplier-costs`
  is `purchases.costs.write` even though the form lives in the supplier drawer (mirror of S5's
  `/web/product-costs`). The page and list fragment stay single `suppliers.read`: they render names
  only (the cost rows live in the drawer), so a suppliers-only principal reads the list and is refused
  the drawer naming `purchases.costs.read` — the S5 product-drawer shape, pinned by
  `the_supplier_drawer_carries_the_double_gate_like_the_product_drawer` in both refusal directions.
- **C: the reorder suggestions (`/api/purchases/suggestions`, `/web/purchases/suggestions`) are
  `inventory.read`**: the suggestion is stock-derived data (current stock vs min/max, resolved supplier
  and satellite cost), not a purchase document; the codes it is built from are `inventory.read` + the
  costs read, and the list the operator scans lives on the inventory side of the boundary. The costs,
  recorded deliberately: a `purchases.read`-only principal is refused the suggestions fragment while
  still seeing suggestions embedded server-side in `/purchases` (see the page note below), and a
  `suppliers.read`-only principal is refused them entirely. No seeded matrix separates the pair
  (`deposito` holds `inventory.read` + `purchases.read` together; admin holds everything).
- **D: creating the purchase order from a suggestion (`POST /web/purchases/from-suggestion`) is
  `purchases.create`**: the suggestion list is read with `inventory.read`, but the document it creates
  is a purchase (draft + its first line in one step), so the creation gate is the recording tier. The
  suggestion is re-derived INSIDE the handler as a service composition — never a permission grant, the
  S6 `resolve_product_ref` shape — so a `purchases.create`-only principal can act on a suggestion it
  could not have listed itself; the cost is that its input arrives from a client it cannot verify, and
  the service re-derives every business value (supplier, qty, cost) from the database anyway.
- **The purchases page `/purchases` is a single `purchases.read` gate.** Deliberate consequence, same
  contract as S6's cross-capability dependency: the page server-renders the reorder suggestions
  (stock-derived, `inventory.read` data) and the supplier roster (`suppliers.read` data) its create
  dialog needs, so a purchases-only principal sees that embedded context; the suggestion fragment and
  the supplier screens themselves refuse it. Seeded purchase holders (`deposito`, admin) hold both, so
  no natural operator is hit; an AND of the codes is the documented future shape, not a new kernel
  type.
- **Confirming a purchase stays `purchases.create`** even when it embeds the cash tender: the
  embedded payment is part of the purchase lifecycle itself (one Expense + payment rows in the same
  step), the same call S6 made for confirming a cash sale.
- **The purchase record page and fragments are a single `purchases.read` gate** (not an AND with the
  costs or suppliers codes): the record renders THAT purchase's own documents — lines with unit cost,
  payments with account and method — which are purchases data; the line costs are purchase data, not
  the per-supplier satellite. Same shape as S6's customer statement.

#### S7 part 1 mutation table (writer round — every gate removed, its test observed FAILING, then restored)

The S5 lesson was enforced from the first pass: every module carries a read-gate test driven by a
principal holding an UNRELATED permission (never the empty set), so a wrong-permission gate cannot pass
for a broken fixture. Mutation method: one gate at a time, test observed failing, gate restored; the
final diff keeps every annotation. 51 removals → 51 observed failures; no gate needed a forced
justification and no handler turned out to declare the wrong permission (the four judgement calls
above survived the mutation review unchanged).

| Cluster | Gates | Test(s) that fail when the gate is removed | Observed failure |
| --- | --- | --- | --- |
| suppliers API reads (list, get) | 2 × `SuppliersRead` | `the_read_gates_refuse_a_principal_without_the_read_permission` (purchases_api) | 200 with the supplier JSON (handler ran) instead of 403 |
| suppliers API entity writes (create, update, activate, deactivate, delete) | 5 × `SuppliersWrite` | `ac10_a_purchases_read_only_principal_reads_and_is_refused_the_writes` | 201/200/400 (handler ran; delete hit the satellite RESTRICT 400) instead of 403 |
| costs API read | `PurchasesCostsRead` | the read-gates test | 200 with the costs JSON instead of 403 |
| costs API write | `PurchasesCostsWrite` | the read-only-principal test | 201 with the recorded cost instead of 403 |
| purchases API reads (list, get) | 2 × `PurchasesRead` | the read-gates test | 200 with the purchases JSON instead of 403 |
| purchases API suggestions | `InventoryRead` | the read-gates test | 200 with the suggestions JSON instead of 403 |
| purchases API draft lifecycle (create, update, add/update/remove line, confirm) | 6 × `PurchasesCreate` | the read-only test; for create, `ac10_a_purchases_refusal_writes_nothing` (row count grows) | handler ran (200/201/204) or its own validation answered instead of 403 |
| purchases API payments (per-purchase + supplier-level) | 2 × `PurchasesCreate` | the write-nothing test | the payment really posted instead of 403 |
| purchases API cancel | `PurchasesCancel` | the write-nothing test | 200, status flipped to Cancelled instead of 403 |
| purchases web reads (page, record, list, detail fragments) | 4 × `PurchasesRead` | `the_read_gates_refuse_a_principal_without_the_read_permission` (purchases_web) | 200 with the page/fragment instead of 403 HTML |
| purchases web suggestions | `InventoryRead` | the read-gates test | 200 with the fragment instead of 403 naming `inventory.read` |
| purchases web create + seed-from-suggestion | 2 × `PurchasesCreate` | the web write-nothing test / the read-only web test | 303 to the new record instead of 403 |
| purchases web path mutations (add/update/remove line, header, confirm, payment, cancel) | 8 × (`PurchasesCreate` / `PurchasesCancel`) | the web read-only test / the write-nothing test | handler ran (fragment 200) instead of 403 |
| purchases web collection adapters (lines, confirm, payments, cancel) | 4 × (own gate) | `the_purchases_collection_adapters_carry_their_own_gate` | the delegated action ran instead of 403 |
| suppliers web reads (page, list fragment, edit form) | 3 × `SuppliersRead` | the read-gates test (suppliers_web) | 200 with the page/fragment instead of 403 HTML |
| supplier drawer double gate | `SuppliersRead` + `PurchasesCostsRead` (2 removals) | `the_read_gates...` (drawer case) / `the_supplier_drawer_carries_the_double_gate_like_the_product_drawer` | drawer rendered (200) instead of 403 naming the removed gate |
| suppliers web entity writes (create, edit, activate, deactivate, delete) | 5 × `SuppliersWrite` | the suppliers read-only test / the write-nothing test | handler ran (fragment/list answer) instead of 403 |
| suppliers web cost record | `PurchasesCostsWrite` | the write-nothing test | the cost row was written instead of 403 |
| suppliers web pay | `PurchasesCreate` | the suppliers read-only test | the payment ran instead of 403 naming `purchases.create` |

New tests (17, named per module): `the_read_gates_refuse_a_principal_without_the_read_permission`,
`ac10_a_purchases_read_only_principal_reads_and_is_refused_the_writes`,
`ac10_a_purchases_refusal_writes_nothing`,
`ac10_the_purchases_holding_principal_gets_the_normal_answer`,
`an_anonymous_request_still_gets_the_json_gate_not_the_permission_refusal` (purchases_api);
`the_read_gates_refuse_a_principal_without_the_read_permission`,
`ac10_a_purchases_read_only_principal_is_refused_the_web_mutations`,
`the_purchases_collection_adapters_carry_their_own_gate`,
`ac10_the_purchases_web_refusal_writes_nothing`,
`ac10_the_purchases_web_holding_principal_gets_the_normal_answer`,
`an_anonymous_request_still_gets_the_login_gate_not_the_permission_refusal` (purchases_web);
`the_read_gates_refuse_a_principal_without_the_read_permission`,
`the_supplier_drawer_carries_the_double_gate_like_the_product_drawer`,
`ac10_a_suppliers_read_only_principal_is_refused_the_web_mutations`,
`ac10_the_suppliers_web_refusal_writes_nothing`,
`ac10_the_suppliers_web_holding_principal_gets_the_normal_answer`,
`an_anonymous_request_still_gets_the_login_gate_not_the_permission_refusal` (suppliers_web).

#### S7 part 1 warning ledger re-measure (2026-09-20, end state)
`cargo check --all-targets` **0 errors, 56 lint warnings (delta 0 vs `main`, measured the ledger way:
58 raw `warning:` lines minus the 2 per-target summary lines)**. The first pass's own test module
exposed one `unused import: PaymentType` warning in `suppliers_web.rs` (the writer's new test imported
what it used inline); fixed by removing the import — the convention held: fix the cause, never add an
attribute. No `#[allow]` attributes added; the dead-code/unused-import allow grep is empty. NO dormant
ledger item graduates by warning count in this slice — the extractor, the refusal shapes and the
principal membership checks already graduated in S3 part 2, and this slice only widens their
production consumers (51 more handlers). Still dormant:
`Principal.{username, display_name, must_change_password}` (S7 part 2 navigation),
`role_repo::{count_active_holders, revoke}`, `Role.{created_at, updated_at}`,
`Permission.{action, created_at}`. The count must still be back at or below 56 by the end of S7, with
no `#[allow]` attributes as the mechanism. `test_support.rs`, `authz.rs`, `models.rs` and `error.rs`
needed NO change this slice: `seed_session_with_permissions` covered every probe the tests needed.

### S7 part 2 — nav gating, the sidebar's truth, and the ledger close — ENTREGADA (writer round) 2026-09-20
- [x] T22 (rest): AC21 delivered — the principal plumbed into every page struct and the sidebar showing
      only the entries that principal may read. The kernel-side nav view lives in `security/authz.rs`:
      `NavEntry`/`NAV_ENTRIES` (the ONE mapping entry → permission, one row per entry with the sidebar
      group) and `Nav` (`for_principal`, the anonymous fail-closed fallback, `visible(key)`,
      `group_visible(group)`). Thirteen full-page template structs carry the `nav` field
      (`web.rs` ×2, `inventory_web.rs`, `sales_web.rs` ×2, `customers_web.rs`, `purchases_web.rs` ×2,
      `suppliers_web.rs`, `users_web.rs`, `roles_web.rs`, the password page in `identity_web.rs`, and the
      refusal card in `authz.rs`); the sidebar's `nav_item` macro gates EVERY entry by construction
      (`{% if nav.visible(key) %}`), so an entry cannot render ungated, and the group headings hide when
      a group has no visible entry. The login page keeps overriding the sidebar block and needs no
      principal. The sidebar shows the signed-in user's display name and username next to the logout
      control (the S3-ii deferral), and the password page says why it confines a flagged session — the
      principal's `must_change_password` field reaching the operator.
- [x] T23 (rest): tests for AC21 (4 new tests, every claim mutation-validated below) — the navigation
      shows exactly the readable entries in both directions, the bootstrap administrator sees all, and
      the drift discipline the permission catalog uses now covers the nav mapping (template keys vs
      `NAV_ENTRIES`, both directions, plus catalog membership).

#### S7 part 2 mapping decisions (written by the writer round of 2026-09-20)
- **The mapping in one place.** `NAV_ENTRIES` declares: dashboard → `dashboard.read`, sales →
  `sales.read`, purchases → `purchases.read`, products → `inventory.read`, suppliers → `suppliers.read`,
  customers → `customers.read`, accounts → `finance.read` (the entry points at the dashboard section;
  the finance reads carry that code), users → `identity.users.read`, roles → `identity.roles.manage`
  (the S4 deliberate tier — no `identity.roles.read` exists), and password → no code (every signed-in
  operator). Each code is the one the entry's route itself declares; no route's permission was re-decided
  in this slice.
- **The suggestion block (part 1's UX item) — the page renders it conditionally, the fragment's gate
  stands.** Chosen: render `/purchases`'s Sugerido block only when the principal holds `inventory.read`
  (the gate the fragment AND the API already carry), keeping the route permission untouched — this
  slice reads permissions, it does not re-decide them, and the suggestion is stock-derived data, so the
  `inventory.read` gate was the right one and the PAGE was the wrong half. When not permitted the page
  renders no block at all (and skips the service read), so a `purchases.read`-only principal can no
  longer see data its own refresh button would refuse. The supplier roster embedded in the create
  dialog stays the recorded S7 part 1 consequence (the dialog needs it to record a purchase).
- **The declared order rule for the double-gated drawers** is now in the spec (see spec.md Rules): the
  extractors run in declaration order and the FIRST one to fail names the refusal; the drawer's
  own-screen read is declared first, the costs read second.
- **Pre-existing tests touched (2, same intent, stronger assertion):**
  `ac10_a_full_page_request_without_the_permission_gets_the_html_refusal` (authz.rs) and
  `ac10_a_plain_browser_mutation_refusal_is_the_html_page` (inventory_web.rs) both asserted the refusal
  page renders `data-nav="dashboard"`. Under AC21 the kernel probe (no permissions) and the
  inventory-only probe no longer see the dashboard entry — the assertion was proving the OPPOSITE of the
  new rule. Updated to `data-nav="password"` (always visible, the shell still renders) and
  `data-nav="products"` respectively — the second now also proves the refusal's nav reflects the
  principal. No assertion of refusal behavior was touched.

#### S7 part 2 mutation table (writer round — every claim observed FAILING, then restored)

| # | Mutation | Test that failed | Observed failure |
| --- | --- | --- | --- |
| M1 | The `nav.visible(key)` gate dropped from the `sales` entry (the template renders it for everyone) | `ac21_a_limited_principal_sees_exactly_the_entries_it_may_read` | the hidden-entry assertion fired: `the entry sales must be hidden from this principal` |
| M2 | `NAV_ENTRIES` maps `products` to `SuppliersRead::CODE` instead of `InventoryRead::CODE` | the same test | the readable-entry assertion fired: `the readable entry products must render` |
| M3 | A `nav_item("reports", ...)` added to the sidebar partial with no declared row | `ac21_every_sidebar_entry_declares_a_catalog_permission` | `sidebar entry "reports" has no declared nav mapping` |
| M4 | The `{% if show_suggestions %}` gate dropped from `/purchases`'s Sugerido section | `ac21_the_suggestions_block_hides_from_a_principal_that_cannot_refresh_it` | `the Sugerido block must not render for a principal the suggestions fragment would refuse` |

#### S7 part 2 warning ledger re-measure (2026-09-20, closing measure — the FIRST graduation since S4)
`cargo check --all-targets` **0 errors, 55 lint warnings (delta −1 vs `main`'s 56; measured the ledger
way: 57 raw `warning:` lines minus the 2 per-target summary lines)**. The count falls for the first time
since S4's 60 → 56. Graduated items (made production-readable by this slice):

| Graduated item | Consumed by S7 part 2 |
| --- | --- |
| `Principal.{username, display_name, must_change_password}` | the sidebar renders display name + username next to logout (`Nav::for_principal`), and the password page reads `must_change_password` to say why the session is confined; the `_PIN_IDENTITY_FIELDS` pin left `authz.rs` (the fields are production-read now, `Principal.user_id` had already graduated with S3 part 2) |

Still dormant for their slices: `role_repo::{count_active_holders, revoke}` (no natural consumer yet —
deletion blocks on the TOTAL holder set via `holder_names`, and grant/revocation lives on the users
screen) and `Role.{created_at, updated_at}`, `Permission.{action, created_at}` (Phase B reads them).
The requirement is met: the count is below 56 with NO `#[allow(dead_code)]` /
`#[allow(unused_imports)]` attributes — `grep -rn 'allow(dead_code)\|allow(unused_imports)' src/` stays
empty. New surface written and consumed at birth (no new warnings): `Nav`, `NavEntry`, `NAV_ENTRIES`,
`Nav::visible`/`group_visible`, the `nav` field on the 13 page structs, and the 7 new tests.

#### S7 part 2 numbers and live probe (2026-09-20)
`cargo test` 604 → **611 passed / 0 failed** (+7: the two kernel drift tests, the two dashboard AC21
tests plus the full-permission test, the suggestions test, the password-notice test — 6 names above,
7 counted: `ac21_every_sidebar_entry_declares_a_catalog_permission`,
`ac21_the_nav_view_shows_exactly_the_readable_entries`,
`ac21_a_limited_principal_sees_exactly_the_entries_it_may_read`,
`ac21_the_full_permission_principal_sees_every_entry`,
`ac21_the_sidebar_shows_the_signed_in_user_next_to_logout`,
`ac21_the_suggestions_block_hides_from_a_principal_that_cannot_refresh_it`,
`the_password_page_says_why_it_confines_a_flagged_session`). `scripts/e2e.sh -k identity` 4 passed,
`-k parties` 9 passed / 1 skipped (the opt-in screenshot probe, not a failure).

Live probe with the real binary (throwaway DB, `ROYA_ADMIN_PASSWORD` set): admin login → the sidebar
renders every entry once (`data-nav` census: accounts, customers, dashboard, password, products,
purchases, roles, sales, suppliers, users — each 1) and the user block shows `Admin`/`admin` above
`Cerrar sesión`. A limited principal built THROUGH THE SCREENS (role `nav_probe` created by
`POST /web/roles`, its matrix set to `purchases.read` + `suppliers.read` by `POST /web/roles/matrix`,
user `navlimit` created by `POST /web/users`, the role assigned by `POST /web/users/roles`, the forced
change lifted by the real `/password` flow): `GET /` → 403 (no `dashboard.read`) whose refusal card
keeps exactly the three entries it may read; `GET /purchases` and `GET /suppliers` → 200 with the same
three entries; `data-nav` census on its pages: password, purchases, suppliers — and NOTHING else; the
group census shows only `operation`, `catalogue`, `account` (no empty `cash` heading); the Sugerido
block is ABSENT from its `/purchases` while the administrator's still renders (2 matches); clicking a
hidden entry is not even offered (no `data-nav="sales"`/`customers`/`products`/`accounts`/`users`
/`roles`/`dashboard` in its markup) and the direct URLs stay refused (`/sales`, `/users`,
`/products` → 403; the supplier drawer → 403 naming `purchases.costs.read`). The administrator still
answers 200 everywhere. Probe scratch removed.

#### S7 part 2 CORRECTION ROUND (2026-09-20): the nav promise the route refused — MAJOR, plus the dormant-ledger NIT
The verification of the delivered S7 part 2 found (M1, MAJOR): the `accounts` nav entry (href
`/#accounts`) mapped to `finance.read` while that href opens the `/` route, whose gate is
`dashboard.read` — a principal holding `finance.read` without `dashboard.read` SAW the entry and got
a 403 clicking it. The old drift test did not see it because it only trusted the table (key declared,
code in the catalog), never the route the href opens.

**The rule (written in spec.md, Navigation):** a nav entry declares EVERY permission it needs — the
gate of the route its href opens, plus the data-owner permission of any block its label names (the
same shape the double-gated drawers and the suggestions block use); less shows a screen the route
refuses, more hides a screen the principal may read.

**How each entry applies it:** `dashboard`/`sales`/`purchases`/`products`/`suppliers`/`customers`/
`users`/`roles` keep their single code (the gate of the route their href opens, no named blocks);
`accounts` now carries `dashboard.read` (the `/` route's gate) AND `finance.read` (the accounts
block's data owner), and the dashboard renders the accounts block conditionally on `finance.read`
(`show_accounts` in `web.rs`, `{% if show_accounts %}` around the card in `dashboard.html` — the
block IS separable, a distinct card, so the balances need no inseparable justification; the rest of
the page stays behind `dashboard.read`, the gate its route declares); `password` declares no codes,
unchanged. `NavEntry.permission` became `permissions: &'static [&'static str]` and `Nav::from_parts`
requires every declared code — the sidebar macro still gates by construction (`nav.visible(key)`),
no special cases.

**The invariant that replaced the table-trusting test:** for every nav entry, a principal holding
exactly the permissions that entry declares gets 200 on that entry's href —
`ac21_a_principal_holding_exactly_what_an_entry_declares_opens_its_href` (authz.rs), one test, every
entry, driven against the real router with the hrefs parsed from the sidebar partial itself. Each
declared code must also be load-bearing: the declared set minus that code either gets the route's 403
or misses the block the label names (test-side marker: `accounts` → `id="accounts"`; no other entry's
label names a block, so any extra code on them has nothing to point at). `password` needs no
exclusion-with-reason: it declares no codes and is verified the same way with the permissionless
signed-in principal. The static direction of the old test survives as
`ac21_the_sidebar_renders_exactly_the_declared_entries_with_catalog_codes` (keys both ways + catalog
membership), which is declaration drift, not behavioral truth.

**Mutation table (both directions, observed FAILING, then restored):**

| Mutation | Observed failure |
| --- | --- |
| M-A: `accounts` declares `[finance.read]` only — LESS than its route requires | `nav entry "accounts" declares ["finance.read"] but GET /#accounts refuses the exact principal — 403 vs 200` (exactly 2026-09-20's bug) |
| M-B: `accounts` declares `[dashboard.read, finance.read, sales.read]` — one code it does not need | `nav entry "accounts" declares sales.read but the href opens and the named block still renders without it: sales.read is over-declared and hides nothing` |

**NIT (dormant-ledger honesty):** the ledger mapped `role_repo::{count_active_holders, revoke}` to
Phase B, whose audit-column work does not consume them. Corrected in the S2 dormant table and the S7
closing ledger state: they are the S2 trait surface exercised by tests (`revoke`: single-grant removal
surfacing the AC14 guard; `count_active_holders`: the AC15 holder count) with NO known consumer —
production grant/revocation lives on the users screen through `replace_user_roles`, deletion blocks on
`holder_names` plus the RESTRICT FK, and Phase B's audit columns will NOT consume them. The ODD log
(odd/tasks/identity-rbac.md) carries the same correction.

**Correction-round numbers:** `cargo test` 611 → **613 passed / 0 failed** (−1:
`ac21_every_sidebar_entry_declares_a_catalog_permission` replaced; +3:
`ac21_a_principal_holding_exactly_what_an_entry_declares_opens_its_href`,
`ac21_the_sidebar_renders_exactly_the_declared_entries_with_catalog_codes`,
`ac21_the_two_code_accounts_entry_shows_only_to_principals_holding_both` — the raw two-code HTML
fragments print under `--nocapture`). `cargo check --all-targets` 0 errors, **55 warnings** (57 raw
`warning:` lines − the 2 per-target summaries; delta 0 vs the S7 close), no `#[allow]` added (grep
empty). `scripts/e2e.sh -k identity` 4 passed; `-k parties` 9 passed / 1 skipped (the opt-in
screenshot probe, not a failure). Two-code probe through the real router: `finance.read` only → GET /
403, no `data-nav="accounts"`; `dashboard.read` only → GET 200, entry AND block absent; both codes →
GET 200, anchor and `id="accounts"` card present.

### S7 — enforcement: purchases, suppliers, identity, dashboard
- [x] T22: `Require<P>` per action (part 1), nav gating (part 2), dashboard and identity screens gated
      (part 1 covered the last department handlers; the password page was already reachable and the
      dashboard was gated in S5).
- [x] T23: tests for AC10 (parts 1), AC21 (part 2), and the full-surface exposure guard: every route's
      read permission is now ALSO the nav mapping's code, the identity screens carry their entries, and
      the part 1 grep of registered handlers is joined by the sidebar's own drift tests.

#### S7 closing ledger state
Dormant with NO known consumer: `role_repo::{count_active_holders, revoke}` (see the corrected S2
row above — Phase B's audit columns will not consume them); dormant for Phase B proper:
`Role.{created_at, updated_at}`, `Permission.{action, created_at}`. The S2 requirement (the count back
at or below 56 by the end of S7, no `#[allow]` as the mechanism) is CLOSED at 55.

### S8 — Phase A close
- [x] T24: browser slice (`e2e/tests/test_identity.py`) for AC22, wired into the harness login step.
      (S8 part 1, writer round on `test/identity-browser-slice`: the four AC22 cases are now browser
      tests — login and the cookie flags were the S1b slice, and this round added the forced password
      change confinement, the session-expiry mid-HTMX navigation, and the permission-denied HTMX form
      reaching the notice box with no swap. FINDING: htmx 1.9.12 DOES honour `HX-Redirect` on the
      guard's `401` — observed in the browser as a real navigation to `/login` with the form visible.
      The session row is expired directly in the throwaway database
      (`helpers.expire_session_in_database`), the suite's one deliberate non-HTTP step: expiry is not
      an action the interface offers. The limited principal (role `solo_consulta` holding exactly
      `sales.read`) is built through the roles and users screens; no Rust file, template or migration
      was touched.)
- [x] T25: README (no-auth section, module table, migrations, environment), `env.example`, and
      `openspec/specs/identity/spec.md` promoted; the change folder archived for Phase A.
      (Done by the close slice, 2026-09-23: `openspec/specs/identity/spec.md` is the present-tense
      capability spec with the complete route → permission table verified against the code;
      `openspec/specs/README.md` gained the M5 row and the transversal-kernel invariant; the five
      department specs gained an Authorization section pointing at that table; the README feature
      list, project structure, migrations 27–29 and the browser-suite count were updated (the env
      table and `env.example` were already correct from S1b-iii and were verified, not rewritten);
      the Phase B scope (T26–T31 below, the actor columns) moved to
      `openspec/changes/2026-09-19-add-actor-audit/` so this archive records only what was
      delivered.)

## WHERE THE FEATURE PAUSES (2026-09-20, after S6) and how to resume
The parent pauses the feature branch after this slice. State of the ledger when work stops:

**Done (Phase A):** S1a (identity kernel), S1b (deny-by-default gate + login/logout + test plumbing),
S2 (RBAC core: catalog, guards, `Require<P>`, effective-permission middleware, drift test),
S3 part 1 (forced password change), S3 part 2 (users administration + the tier-rule correction
round), S4 (roles administration + permission matrix + its correction round), S5 (enforcement:
finance + inventory), **S6 (enforcement: sales + customers — this slice; nav-gating deliberately
deferred to S7)**.

**Remaining (in order, one PR each):**
1. **S7 — enforcement: purchases, suppliers, identity, dashboard** (T22–T23): the LAST enforcement
   slice; includes the sidebar/navigation hiding by permission (AC21 — needs the principal plumbed
   into every page struct, the reason S5/S6/S7 leave the sidebar alone) and the ledger re-measure
   (`cargo check --all-targets` back to ≤ 56 with NO `#[allow]` as the mechanism; 56 is what
   `main` measures — see the S5 warning ledger correction).
2. **S8 — Phase A close** (T24–T25): the browser slice for AC22 and the two browser debts carried
   since S1b/S3 — the session-expiry `HX-Redirect` mid-HTMX case and the permission-denied HTMX
   form, still unproven in a real browser — plus README (no-auth section, module table,
   migrations, env vars), `env.example`, promoting `openspec/specs/identity/spec.md` and archiving
   the change folder for Phase A.
3. **Phase B — audit of the actor per department** (T26–T31): `created_by`/`updated_by` plumbing
   and display, department by department, ending in the spec's audit section and archive.

**Concrete human follow-ups on pause:**
- **S7 part 1 (`feat/enforcement-purchases-suppliers`, writer round 2026-09-20) is DONE but has NO
  work-unit commit yet: the writer does not commit; the parent owns git state — commit it as one work
  unit. The pause map below still reads as of after S6; on top of it, S7 part 1 landed the 51
  purchases/suppliers gates with their mutation-validated tests (see the S7 part 1 sections above).
  **S7 part 2 (`feat/nav-gating`, writer round 2026-09-20) is ALSO DONE, in the same uncommitted
  state: nav gating (AC21 — the principal plumbed into the 13 page structs, the sidebar rendering only
  the readable entries), the signed-in user in the sidebar, the suggestion-block alignment, the
  declared-order rule in the spec, the drift tests, and the ledger closed at 55 warnings (≤ 56 met,
  no `#[allow]`). The whole S7 is now ready for its independent verification and its work-unit
  commit(s).**
- The remote branch `feat/roles-administration` still exists on `origin` although its PR #48 is
  already merged into `origin/main` (verified with `git branch -a` on 2026-09-19). Decided: leave it
  un-deleted for now; deleting it is the owner's call, it holds nothing unmerged.
- This slice (S6, `feat/enforcement-sales-customers`) has NO work-unit commit yet: the writer does
  not commit; the parent owns git state — commit it as one work unit before retargeting anything.
- On resume: `mem_context` + project/feature-scoped `mem_search`, then read
  `odd/tasks/identity-rbac.md` and this change folder; the next unfinished task is S7 (T22).

## Phase B — audit

> Phase B no longer belongs to this change: it was carried out, untouched, to
> `openspec/changes/2026-09-19-add-actor-audit/` (proposal, spec, tasks) when Phase A closed, so
> the archive records only what was delivered. The rows below are kept unchecked as the record of
> what moved; do not work from this folder.
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
- [x] On merge of Phase A: create `openspec/specs/identity/spec.md` from AC1-AC17 and AC20-AC23, update the
      `verification` capability with the new browser cases, and move this change's Phase A scope to
      `openspec/changes/archive/`.
- [ ] On merge of Phase B: extend `openspec/specs/identity/spec.md` with the audit rules and AC18-AC19, and
      archive the remaining scope.
