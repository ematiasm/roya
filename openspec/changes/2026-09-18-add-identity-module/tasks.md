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

### S1b — the wiring (the one unavoidably large slice)
- [ ] T6: `security/guard.rs` middleware with the public allowlist and the three refusal shapes.
- [ ] T6b: remove every temporary `#[allow(dead_code)]` / `#[allow(unused_imports)]` attribute that S1a
      added (15 of them, in `security/mod.rs`, `repositories/{user,session}_repo.rs`, `services/identity.rs`,
      `models.rs`, `error.rs`, `repositories/mod.rs`, `services/mod.rs`): once the router consumes the kernel
      they are no longer honest, and the slice is not done until `grep -rn 'allow(dead_code)\|allow(unused_imports)'
      src/security src/services/identity.rs src/repositories/user_repo.rs src/repositories/session_repo.rs`
      returns only genuinely justified entries (S1a suppressed 48 warnings with them, so leaving them is
      leaving 48 warnings hidden).
- [ ] T7: `identity_web.rs` (`GET /login`, `POST /login`, `POST /logout`) and `identity_api.rs`
      (`POST`/`DELETE /api/sessions`), the login template, `AppState` and `main.rs` wiring, environment
      variables, CORS narrowed from the wildcard.
- [ ] T8: `security/test_support.rs` (the shared authentication helper for tests) plus the cookie plumbing
      in the ~160 existing HTTP tests; tests for AC2, AC3, AC20, AC24.

### S2 — RBAC core
- [ ] T9: migration `create_identity_rbac` (catalog, seeded roles, guarded inserts) and
      `create_identity_guards` (the lockout triggers).
- [ ] T10: `security/authz.rs`: `Principal`, the permission catalog in code, the `Permission` trait and
      marker types, `Require<P>` extractor, 403 in both shapes.
- [ ] T11: role and permission repositories, `IdentityService` permission resolution per request, and the
      catalog-drift test.
- [ ] T12: tests for AC10-AC12, AC19 (partial: the identity side), AC20.

### S3 — users administration
- [ ] T13: `/users` list with roles and state, create, deactivate/activate, admin password reset, role
      assignment with `granted_by`, and the interface explanation of every trigger refusal.
- [ ] T14: `GET`/`POST /password` and the `must_change_password` gate in the middleware.
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
