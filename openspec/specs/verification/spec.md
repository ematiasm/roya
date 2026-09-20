# Capability: verification

## Purpose
State how this project decides that something works, because a green suite on its own has twice proven
nothing.

## The two layers, and the boundary between them
```
Rust suite        cargo test         613 tests, about 14 s
Browser suite     scripts/e2e.sh     62 tests, about 90 s
```

**The Rust suite** covers the server: business rules, guards, derived state, error mapping, the rendered
wiring of every page, and the HTTP flows a client would drive. It is fast and thorough and it is the right
place for anything provable by reading a response.

**The browser suite** covers what only a browser can judge: interaction, focus, keyboard navigation,
dialogs, what the URL does across history, and anything htmx decides on the client. It exists because a
defect shipped where **the server response was correct and the interface still showed nothing**.

**The boundary is a rule, not a preference.** A behaviour that only a browser can see belongs in the
browser suite, and must not be approximated by a slower Rust test that checks an attribute and calls it
verified. Equally, the browser suite must not duplicate business rules that the Rust suite already proves:
that would make the slower layer the one people skip.

The identity gate is the worked example (AC22, in `e2e/tests/test_identity.py`): login, the forced
password change, session expiry during an HTMX request and a permission-denied HTMX form are browser
cases. The Rust suite proves each refusal's headers and status in-process; what only a browser can
assert is that htmx 1.9.12 performs the `HX-Redirect` navigation into the login page mid-request, that
the confinement bounces a URL the operator typed by hand, and that a refused form reaches the operator
as the notice box while the DOM never swaps as if it had succeeded.

## Harness contract
- One command: `scripts/e2e.sh`.
- It builds the application once, then spawns the real binary per test against a throwaway database on a
  free port, and polls for readiness rather than sleeping.
- **It proves its own isolation.** The spawned server's log must name the throwaway database, and the
  development database must keep its size and mtime. Without that guard, a misconfigured `DATABASE_URL`
  would seed the real database while the suite passed.
- Data is seeded through the HTTP API, and every seed step reads its effect back, so a silent no-op fails
  where the cause is.
- A failing test leaves a trace, a screenshot and the captured server log; a green run leaves nothing.
- No fixed-duration sleeps. The only exceptions are bounded polling loops for readiness and port release,
  and a deliberate route hold used to observe a transient state.

## Toolchain
Python managed by `uv`, using Playwright, because the repository has **no Node toolchain to install or
manage**: Tailwind is built from its standalone binary precisely so `cargo run` needs no package manager.
Playwright's Python driver still bundles its own Node runtime inside the suite's virtual environment, so
the project is not Node-free on disk; it is Node-free to set up. Chromium is a one-time download outside
the repository (`--only-shell` fetches just the headless shell), and is not needed to run the application.

## The rule that makes any of this mean something
**A regression test is validated by reintroducing the bug and watching the test fail.** Three times in this
project a test that looked like a guard turned out to be decoration: the sales forms that posted to a dead
URL while every test passed, the search results that never rendered while the response was correct, and a
guard rule that was silently relaxed with a flag. A green suite is evidence only after it has been seen
red.

## Known limits, stated so nobody over-reads them
- No assistive technology has been run: accessibility claims rest on the DOM and the live regions a reader
  consumes, not on a screen reader's output.
- Microsoft's browsers, Firefox, Safari and mobile viewports are not covered.
- No CI: both suites run locally.
- htmx needs a few milliseconds to wire an out-of-band replacement. A keystroke landing inside that window
  would trigger a native submit. It is measured, it is below human reaction time, and it is not user
  reachable in practice, but it is real.

## Verification
`src/smoke_tests.rs` for the Rust layer, `e2e/tests/` for the browser layer, and `scripts/e2e.sh` plus
`cargo test` as the two commands. The interface slices that this capability was built to protect live in
`openspec/specs/` alongside it.
