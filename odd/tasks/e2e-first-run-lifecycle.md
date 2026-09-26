# A real first-run lifecycle for the e2e harness

## Objective

Let the e2e harness hold a server whose first-run setup has not been completed, so the setup wizard can be captured by the visual baseline like any other page, and so the wizard's computed-style assertions run against a genuine fresh installation instead of a mid-test database mutation.

## Problem

The `live_server` fixture spawns a server and then immediately completes the real first-run form (`e2e/conftest.py:275`) before any test body runs. `/setup` is one-time only, so it is unreachable through that fixture: `GET /setup` answers `303` to `/login` once setup is done, and in the browser a visit lands on the dashboard with status 200.

The workaround built in the stylesheet feature is `reopen_first_run_setup_in_database`, which deletes the singleton `business_settings` row from the throwaway database mid-test. It works — the real route, template, stylesheet and browser are all exercised — but it is a lifecycle fiction. The capture would be of a wizard on a server that already has a completed install's session, login history and audit rows, reached by removing the row that made the wizard available.

The consequence is that `/setup` has five asserted classes and **zero snapshot coverage**: its other 60 fingerprinted elements, its hover dimension and every class that does not declare a recorded property are unchecked.

## Why

- A baseline that cannot visit a page is not protection, and the repository has already been bitten by exactly this: a green net over unvisited pages is indistinguishable from a correct one.
- The wizard is the only page a first-time operator sees. It deserves the same net as the rest of the application.
- A fixture that can represent "not yet set up" is reusable infrastructure. Completing the wizard, testing its validation, and testing the one-time refusal all need the same state, and today none of them has a home.

## Decisions

- **Extract the spawn, keep the behavior.** The server-spawning body of `live_server` becomes a lower-level fixture that does not complete setup. `live_server` is then that fixture plus setup plus login, with behavior byte-for-byte identical, so all 112 existing e2e tests are unaffected and are expected to pass unchanged.
- **A separate `first_run_server` fixture** represents a genuine fresh installation: no setup row, no session, no login. It is function-scoped, uses its own `tmp_path` database and its own free port, and inherits the same throwaway-database guard.
- **The wizard's computed-style test moves to `first_run_server`,** which makes the `DELETE FROM business_settings` mutation unnecessary. The harness then performs **no** deliberate database write in the setup path at all. The existing `expire_session_in_database` helper is untouched.
- **The snapshot and the computed-style assertions are complementary, not alternatives.** The snapshot records computed styles for the properties the net already tracks, which does not include the properties most of the fifteen rebuilt classes declare. Closing snapshot coverage does not replace any existing assertion, and the computed-style test stays.
- **Containment is the evidence.** The existing captures must remain byte-identical; only new names may appear. A new capture name added without a baseline entry must still fail, which is the property the verify path already guarantees.
- **The throwaway-database guard is applied around the whole lifecycle**, and the dev database must be provably untouched.
- Technical artifacts, code, comments, tests, and UI copy are English.

## Authorized scope

Repository-local e2e harness, baseline, test, and feature-document changes. No push, PR creation, or merge without explicit user request. No change to the application: no Rust source, no template, no stylesheet, no migration, and no change to the development database.

## Route declaration

One bounded delegated-direct ODD work unit. It touches the fixture decomposition, one new fixture, the existing setup test's wiring, the baseline's page list, and the docs — multiple non-trivial files, so one writer. The parent owns task closure, the work-unit commit, verification, and delivery reporting. No SDD phase is created.

## TDD mode

- Effective mode: strict TDD inherited from project configuration (`openspec/config.yaml`, `strict_tdd: true`).
- Test runner: `cargo test`, plus the Playwright harness.
- The RED is a test that asserts a `/setup` capture exists in the baseline; the verify path must fail before the capture is added.

## Work units

- [x] U1 — Decompose the server fixture and add `/setup` to the visual baseline.
  - **Two REDs, and the first one was a design bug, not a missing entry.** The first attempt built `live_server` on the pending-install fixture. A test asking for both got the **same server** — pytest caches a fixture per test — so the browser landed on `/login` and the run failed there: `Page URL expected to be '…/setup'`, actual `…/login`. That is why `first_run_server` is an **independent** spawn, and why the URL assertion is load-bearing rather than decoration. It is also why each spawn gets its own database **file name**: two servers sharing one SQLite file would share the completed install's `business_settings` row and the wizard would vanish again. The `label` parameter is the only thing preventing that, so the guard proves "a throwaway file", never "its own throwaway file"; the failure mode is loud — the first-run server answers 303 and the URL assertion fires.
  - The second RED was the intended one: `bash scripts/e2e.sh tests/test_visual_baseline.py` → **1 failed, 2 passed**, `AssertionError: the interface changed`, and **every** reported difference was `setup: element appeared at …`. A missing entry, not a crash and not a dashboard. The baseline was then regenerated exactly once.
  - The decomposition: `_spawned_server(binary, tmp_path, label)` is a context manager holding the whole spawn lifecycle; `_pending_install_server` spawns without completing setup; `live_server` is that plus setup plus login in the original order; `first_run_server` is an independent second spawn with no session; `first_run_page` is the cookie-free page for it.
  - **Server isolation verified, not assumed.** Measured in one process: the two fixtures get different ports, different database files, different inodes, different logs; `first_run_server` has 0 `business_settings` rows and 0 `sessions`, and a login attempt against it is **refused with 503 `application setup required`** rather than merely unused. `live_server` has one of each and answers `GET /setup` with 303.
  - `live_server` is unchanged in order and behavior: readiness, throwaway guard, setup, login — the same four steps in the same sequence, and the full suite is 112 passed / 4 skipped both before and after.
  - The wizard's computed-style test moved to `first_run_server`, and `reopen_first_run_setup_in_database` was **deleted** after a repo-wide search proved it unreferenced. The setup path now performs no database write at all. The wizard's assertion tables and helpers are byte-identical to `HEAD`; nothing was weakened, dropped or reordered to make a capture pass.
  - **Failure artifacts were restored in a follow-up round, because the move silently cost them.** `first_run_page` had lost its trace, screenshot and server log, and because both fixtures finalize into the same per-test artifact directory in reverse setup order, the live server's log was overwriting the first-run one's — along with `trace.zip` and `screenshot.png`. `_write_failure_artifacts` gained a `prefix` (defaulting to `""`, which `test_harness.py` asserts on) and the wizard test now traces. Verified by forcing failures: the wizard failure produced `first-run-screenshot.png`, `first-run-server.log` and `first-run-trace.zip`, with the log naming `roya-first-run.db` and the trace containing real `/setup` requests; a `/setup` baseline failure left all six files with each log naming its own database.
  - Evidence: RED/GREEN as quoted; `bash scripts/e2e.sh tests/test_visual_baseline.py` → 3 passed; `tests/test_settings.py` → 6 passed; full `bash scripts/e2e.sh` → 112 passed, 4 skipped, unchanged before and after; `cargo test` → 1117 passed; `cargo test stylesheet` → 10 passed; `git diff --check` clean. Containment: 83 pre-existing capture names, **all byte-identical**, 0 removed, 0 changed, 3 added, compared by per-name `sha256` over canonical JSON — the file is one line, so `git diff --stat` is not evidence. The four skips are the pre-existing opt-in probes at `test_harness.py:66` and `:81`, `test_parties.py:513` and `test_products.py:1403`. Commit identity is recorded in this document after the work-unit commit.

## Acceptance criteria

1. A fixture exists that serves a completed-install-pending application.
2. `live_server` behavior is unchanged and every pre-existing e2e test still passes.
3. `/setup` is captured by the visual baseline, including its hover dimension if the suite's conventions require it.
4. Every pre-existing capture is byte-identical, proven by per-name hash.
5. The wizard's computed-style assertions run against `first_run_server`, not against a mutated completed install.
6. The setup path performs no database write at all, and the unreferenced helper is removed rather than left as dead code.
7. The wizard's existing computed-style assertions are not weakened, dropped or reordered to make a capture pass.
8. The dev database is provably untouched, and the throwaway guard covers both fixtures.
9. A `/setup` capture without a baseline entry still fails, so the net cannot be satisfied by adding a page without recording it.
10. Focused tests, the full Rust suite, and the full browser suite pass, with all skips recorded.

## Applicable checks

- `bash scripts/e2e.sh tests/test_visual_baseline.py`
- `bash scripts/e2e.sh tests/test_settings.py`
- `bash scripts/e2e.sh` (full)
- `cargo test stylesheet`
- `cargo test`
- `git diff --check`

## Progress and evidence

- Baseline: branch `feat/tax-calculation-settings` at `b6e6285`, working tree clean apart from the unrelated untracked `odd/tasks/pos-counter-sales.md`, which must never be staged by this task.
- Verified: `live_server` completes setup inside the fixture (`e2e/conftest.py:275`), so `/setup` is unreachable through it. `reopen_first_run_setup_in_database` exists as the current workaround and is this unit's thing to retire.
- Feature document: `odd/tasks/e2e-first-run-lifecycle.md`.
- Engram mirror topic: `odd/e2e-first-run-lifecycle/tasks`.
- The stylesheet feature's known-gap entry about this gap is closed by U1; the parent records the closing pointer in that document.
- U1 delivery: work-unit commit `a351e76` (`test(e2e): capture the first-run wizard on a real fresh install`), which carries the fixture decomposition, `first_run_server` and `first_run_page`, the `/setup` capture trio, the restored failure artifacts, the deletion of `reopen_first_run_setup_in_database`, and this document. Rollback boundary: reverting that one commit removes the two new fixtures, the `/setup` capture trio and the artifact prefix, and restores the mutation helper — no application source, template, stylesheet or migration is involved, so the stylesheet feature and every other e2e test are untouched by a revert.

## Gotchas discovered here

- **A log line does not identify a server's state.** Both servers print `initial setup required: open /setup…` at boot, because `live_server`'s setup is pending until the harness posts the form. The only unambiguous discriminator in a server log is its `database_url`. Treating that boot line as proof of a fresh install would have been a false claim.
- **Two pytest fixtures are not two servers if one is built on the other.** The per-test cache returns the same instance, which silently gave both requests the same server. Independent spawns are required, not merely cleaner.
- **One SQLite file, two servers, one `business_settings` row.** Splitting ports is not enough; the file has to be distinct too.
- When two fixtures finalize into the same per-test artifact directory, teardown order decides which evidence survives. The later fixture overwrites the earlier one's files.

## Delivered

Shipped 2026-09-26 as part of PR [#111](https://github.com/ematiasm/roya/pull/111), merged as `be57b39` with `size:exception`.

| Work unit | Implementation on `main` | Evidence on `main` | Pre-rebase, local only |
|-----------|---------------------------|---------------------|-------------------------|
| U1 | `407fb17` | `2b0028d` | `a351e76`, `bfdb5e6` |

This branch was rebased onto `main` after PR #110 merged, so the SHAs cited above are the pre-rebase ones and resolve only in a local clone of the deleted feature branch. The mapping was produced by matching commit subjects, not by hand.

This document also cites one pre-rebase identity of a **sibling** slice: `b6e6285` is the pre-rebase evidence commit of `odd/tasks/tailwind-stylesheet-rebuild.md`, delivered as `3c1f73b` in this same PR.
