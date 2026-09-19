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

**Requirement:** the count must be back at or below 58 by the end of S7, with **no
`#[allow(dead_code)]` / `#[allow(unused_imports)]` attributes as the mechanism**: each
consuming slice makes its surface reachable (S3 and S4 cover the repositories and
service methods, S5-S7 cover the extractor, the refusal shapes and the principal
reads), and S7's closing check re-runs `cargo check --all-targets`.

### S3 — users administration
- [ ] T13: `/users` list with roles and state, create, deactivate/activate, admin password reset, role
      assignment with `granted_by`, and the interface explanation of every trigger refusal.
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
- [ ] T15: tests for AC13-AC16, AC21 (users surface).

### S4 — roles administration
- [ ] T16: `/roles` list, create, edit, delete, and the permission matrix per module and action.
- [ ] T17: tests for AC17, AC13 (through the interface), AC15.

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
