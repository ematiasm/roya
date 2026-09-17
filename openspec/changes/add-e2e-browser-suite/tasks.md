# Tasks: add-e2e-browser-suite

## Review Workload Forecast
- Estimated: ~600-800 lines in total, almost all Python and one shell script. No Rust changes until E3
  fixes templates.
- This change adds a second toolchain, so the first slice carries the reasoning in the README as well as
  the code.
- Chained PRs recommended: Yes, one slice at a time. E1 alone is a coherent unit worth reviewing.
- 400-line budget risk: Low for E1 and E2, moderate for E3 because it touches templates.
- Decision needed before apply: No. The tooling was decided before this change was written and the open
  questions are resolved below.

## Decisions carried in from the plan reconciliation
- **The plan lives here**, in OpenSpec, not in `odd/tasks`, which this repository has never used.
- **No `HOST` environment variable is needed.** `PORT` and `DATABASE_URL` already exist, and the current
  bind is kept because the counter mode will want the application reachable from a tablet on the LAN.
- **The tree is stable.** The interface slices that were in flight are committed, so the suite is not
  written against a moving target.
- **Scope covers both the picker and the filters**, since both are committed and stable.

## Slice E1 — the harness
- [x] T1: `e2e/pyproject.toml` cleaned up: real description, dependencies pinned, `uv.lock` committed,
      the placeholder module removed
- [x] T2: `conftest.py`: build once, spawn the real binary per test against a throwaway database on a free
      port, wait by polling rather than sleeping, tear down and release the port
- [x] T3: failure artifacts: trace, screenshot and the captured server log, kept only for failed tests
- [x] T4: `helpers.py`: seed an account with payment methods, a product with barcode and stock, a customer,
      a supplier with a product cost, and documents with lines, all through the HTTP API
- [x] T5: a first passing test that proves the harness works end to end: the dashboard loads and a scan
      adds a line
- [x] T6: `scripts/e2e.sh` as the one command, and `e2e/README.md` explaining how to run it, what it covers
      and what it deliberately does not
- [x] T7: the root README gains a short section on the browser suite and the toolchain it brings

## Slice E2 — the critical flows
- [x] T8: picker tests: scanning adds in one interaction and the field returns empty and focused
      (AC3); choosing from the results carries the typed quantity (AC4)
- [x] T9: filter tests: each criterion narrows the list, and a filter combined with another still narrows
- [x] T10: confirmation tests: dismissing the dialog leaves the sale untouched, accepting cancels it (AC8)
- [x] T11: the results announce the match count to assistive technology (AC9)

## Slice E3 — the three defects, closed
- [x] T12: write the test that fails because filtering does not update the URL, show it failing, then fix
      the templates so a filtered view can be reloaded and shared (AC5)
- [x] T13: write the test that fails because the results cannot be traversed with the arrow keys, show it
      failing, then implement it (AC6)
- [x] T14: write the test that fails because a search in flight looks like an empty result, show it
      failing, then add the busy state (AC7)
- [x] T15: re-run the whole suite and the Rust suite

## Verify
- [x] AC1, AC2, AC10, AC11, AC12 checked: one command, the development database untouched and the port
      released, a deliberately failing test producing an openable trace, no fixed sleeps, and `cargo test`
      unaffected
- [x] Confirm the suite leaves no artifact behind on a green run

## Archiving
- [ ] On merge: add a presentation section to the canonical specs describing the browser suite's boundary
      (what it covers and what it explicitly leaves to the Rust suite), and move this change to
      `openspec/changes/archive/`.
