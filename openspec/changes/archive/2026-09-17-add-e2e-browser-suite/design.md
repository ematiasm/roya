# Design: add-e2e-browser-suite

## Layout
```
e2e/
  pyproject.toml      managed by uv, holds the pinned test dependencies
  uv.lock             committed, so the environment is reproducible
  conftest.py         server, database, browser and artifact fixtures
  helpers.py          seeding helpers that call the HTTP API
  tests/
    test_picker.py        scanning, choosing, focus, quantity
    test_filters.py       narrowing, the URL, reloading
    test_confirmation.py  dialogs around destructive actions
  README.md           how to run it, and what it does not cover
scripts/e2e.sh        the one command
```
`e2e/.venv`, `e2e/.artifacts`, `e2e/__pycache__` and `e2e/.pytest_cache` are already ignored.

## Server fixture
- Build once per session with `cargo build`, then spawn the binary directly rather than `cargo run`, so the
  test run is not at the mercy of a build holding the terminal.
- A throwaway database per test function: a fresh SQLite file in a temporary directory, so a test cannot
  inherit another test's rows. The application applies its migrations at startup, which is fast enough to
  pay per test and removes all cross-test coupling.
- A free port obtained by binding port zero and reading the assignment, then closing. The small race that
  follows is acceptable on a developer machine and is noted rather than hidden.
- Readiness by polling the root until it answers, with a bounded timeout, rather than by sleeping.
- Teardown terminates the process and waits. The server's output is captured and attached to the failure
  artifacts, because "the page did not load" is usually answered by the server log.

## Browser fixture
- `pytest-playwright` supplies the browser and page lifecycle.
- Tracing is enabled per test and kept only when the test fails, together with a screenshot, under
  `e2e/.artifacts/<test name>/`. A green run leaves nothing behind.
- Headless only. A headed mode is a debugging option documented in the README, not a mode the suite runs.

## Waiting policy
No fixed sleeps, anywhere. Playwright's auto-waiting and explicit expectations cover rendering and
navigation, and the search's debounce is handled by waiting for the results to appear rather than by
sleeping past it. This is what keeps the suite honest: a sleep turns a real failure into a slow pass.

## Seeding
Helpers in `helpers.py` create data through the HTTP API: an account with its payment methods, a product
with a barcode and stock, a customer, a supplier with a product cost, and sales and purchases with their
lines. Creating through the API keeps the seed on the same path a user takes, which means a broken
endpoint breaks the seed loudly instead of being papered over by direct SQL.

## Selector policy
The interface already exposes stable ids for the picker, its input, its results container and its results
status; tests use those. Text is asserted only where text is the thing being tested, such as the confirm
dialog's wording. If a flow needs a hook that does not exist, the hook is added to the template
deliberately and named here, rather than binding a test to a class name that a restyle would break.

## Tradeoffs
| Option | Chosen | Why / cost |
|---|---|---|
| Python with `uv` vs Playwright for Node | Python | Keeps the repo free of a Node toolchain, which Tailwind was already shaped to avoid; cost: two languages in the repo, documented in the README |
| Playwright vs Selenium | Playwright | Auto-waiting removes most flakiness and the trace viewer makes a failure diagnosable; cost: a one-time browser download |
| Fresh database per test vs one per session | Per test | Total isolation and no ordering coupling; cost: migrations run per test, acceptable at this size |
| Seeding through the API vs direct SQL | Through the API | The seed exercises the same path a user takes; cost: slightly slower, and a broken endpoint fails the seed |
| Inside `cargo test` vs a separate command | Separate | Keeps the Rust suite fast and free of a browser dependency; cost: one more command to remember, which the README and the script address |
| Headless only | Headless | Deterministic in a terminal; cost: a visual check still requires running the app by hand |

## What this suite does not claim
It does not verify business rules: those stay in the Rust suite, which is faster and already thorough. It
covers the layer where a browser is the only honest judge: interaction, focus, navigation, dialogs and
what the URL does. Keeping that boundary sharp is what stops the browser suite from becoming a slow
duplicate of tests that already exist.
