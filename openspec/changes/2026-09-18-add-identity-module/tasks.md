# Tasks: add-identity-module

## Review Workload Forecast
- Estimated: ~4,500 lines across 14 slices — Phase A (authorization) ~2,600 lines in 8 slices, Phase B
  (audit) ~1,900 lines in 6 slices. Slices are chained branches, one PR each.
- Chained PRs recommended: **Yes — every slice.**
- 400-line budget risk: **High** for every slice, and extreme if Phase A were attempted as one PR.
- Decision needed before apply: **Yes** — the user decides whether Phase B runs immediately after Phase A
  or as a separate program.

## Phase A — authorization

### S1 — authentication foundation
- [ ] T1: dependencies (`argon2`, and the already-locked `sha2`, `getrandom`, `base64`), `security/password.rs`
      with the production parameters and a light test hasher, plus the test that pins the production cost.
- [ ] T2: migrations `create_identity_users` and `create_identity_sessions` with the triggers.
- [ ] T3: models `User`, `NewUser`, `Session`, `Principal` (kernel-side) + `error.rs` variants for
      unauthorized/forbidden.
- [ ] T4: `UserRepository` and `SessionRepository` traits and SQLite impls (validity decided in SQL).
- [ ] T5: `AuthService`: bootstrap admin, login with constant-time verification and the generic failure,
      in-memory throttle with an injected clock, session mint/resolve/renew/revoke, logout.
- [ ] T6: `security/session.rs` cookie read/write with the flags, `security/guard.rs` middleware with the
      public allowlist and the three refusal shapes.
- [ ] T7: `identity_web.rs` (`GET /login`, `POST /login`, `POST /logout`) and `identity_api.rs`
      (`POST`/`DELETE /api/sessions`), templates for the login page, `AppState` and `main.rs` wiring,
      environment variables, CORS narrowed.
- [ ] T8: the authenticated route-test helper and every existing test adapted; tests for AC1-AC9, AC20, AC23.

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
