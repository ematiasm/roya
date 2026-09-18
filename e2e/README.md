# roya browser suite

A small end-to-end suite that drives the real interface in a real browser. It
exists because some behaviour is only honest in a browser: interaction, focus,
navigation, dialogs and what the URL does. Everything provable over HTTP stays
in the Rust smoke suite, which is faster.

## Why Python and Playwright

The repository deliberately has no Node toolchain to install or manage: Tailwind
is built from its standalone binary and the assets are vendored, so `cargo run`
needs no package manager. Playwright for Python keeps the same auto-waiting API
and the same trace viewer without adding a Node project — but it is not Node-free
on disk: the Playwright driver inside `e2e/.venv` bundles its own Node runtime
(the `node` binary alone is about 126 MB). `uv` keeps the Python environment
inside `e2e/.venv` and never touches the system interpreter.

## One-time setup

1. Install [`uv`](https://docs.astral.sh/uv/) (any recent version).
2. Download Chromium once, outside the repository (about 660 MB here). The full
   `chromium` install brings the complete browser (~390 MB), the headless shell
   (~260 MB) and ffmpeg (~5 MB):

   ```bash
   cd e2e
   uv run playwright install chromium
   ```

   The suite always runs headless, so `uv run playwright install chromium
   --only-shell` downloads just the headless shell instead of the full browser
   (about 260 MB); `--headed` debugging then needs the full install. The
   application itself does not need the browser, and the suite needs no network
   access after this download.

## Run it

From the repository root:

```bash
scripts/e2e.sh                 # the whole suite, headless
scripts/e2e.sh -k scan         # one test
scripts/e2e.sh --headed        # run with a visible browser
```

`scripts/e2e.sh` needs nothing installed or managed for Node: it builds the Rust
binary once (via `cargo build`), then spawns that binary directly. The browser
driver's own bundled Node runtime (inside `e2e/.venv`) still runs, as noted above.

Headed mode is a debugging option for a person watching a failure. The suite
itself always runs headless; nothing in the repository depends on `--headed`.

## What it does

- Builds the application once per session and spawns the real binary.
- Gives every test its own throwaway SQLite file in a temporary directory and
  its own free port, obtained by binding port zero. The development database
  `roya.db` is never opened, and the harness proves it: after readiness the
  server's own log must name the throwaway file, the file must exist, and
  `roya.db` must keep the same size and mtime.
- Polls the application until it answers instead of sleeping for a guessed
  time, and proves the port is released when the server stops.
- Seeds data through the HTTP API, on the same endpoints a browser calls, so a
  broken endpoint fails the seed loudly instead of being papered over; each seed
  step also reads back the effect, so a 2xx that stored nothing still fails.
- Uses Playwright's auto-waiting and explicit expectations. No fixed-duration
  sleep ever stands in for a condition: a fixed sleep turns a real failure into a
  slow pass. The only sleeps are the two bounded polling loops that re-check a
  condition every 100 ms — server readiness and port release — and time out.

## What it covers

Slice E1 is the harness: `tests/test_harness.py` proves the isolated server starts
and releases its port, a failure leaves openable evidence, and the seed helpers
refuse a silent no-op.

Slice E2 is the critical flows:

- `tests/test_picker.py` — a barcode scan adds a line in one interaction and the
  field comes back empty and focused; choosing a result adds the product with the
  quantity already typed, on both the sale and the purchase record; a value with no
  exact match is refused with a non-blocking message and no line added; the results
  announce their match count in a live region; a search is not announced as an added
  line.
- `tests/test_filters.py` — each sales-list criterion (status, party, document
  number, date range) narrows the list on its own, two together narrow it further,
  and a filter that matches nothing shows the empty state. The URL a filter should
  carry is deliberately not asserted here: it is slice E3's subject.
- `tests/test_confirmation.py` — cancelling a sale and discarding a draft ask
  first; dismissing leaves the record untouched (asserted on the page and through
  the API), accepting performs the cancellation.

Slice E3 is the three defects that motivated the suite, in `tests/test_search_ux.py`:

- the filter URL — filtering sales, purchases and products pushes the page URL
  (`/sales?status=…`), reloading that URL reproduces the filtered view, clearing the
  filters returns to the bare URL, and Back restores the previous view with the form
  agreeing with the restored URL;
- keyboard traversal — the arrow keys move **real focus** into and through the result
  buttons (so Enter activates the focused result on its own and a screen reader
  follows), ArrowUp walks back to the field, and Escape returns to the field and clears
  it while the announced match count keeps working;
- the busy state — a search in flight shows the `#product-search-busy` status instead
  of an empty area. The test holds the response with Playwright route interception so
  the transient state can be observed, then releases it; it does not sleep.

The picker macro deliberately exposes `#product-search-busy` as the busy hook that
htmx's `hx-indicator` toggles; the stylesheet hides it with `display:none` until the
request is in flight. It is only the visual cue (`aria-hidden`): the announced state
lives in `#product-search-status`, the text of the single polite region
`#product-search-results`, which says "Searching…" in flight and the count when the
results land.

A follow-up round closed three more defects in that keyboard flow, also in
`tests/test_search_ux.py`:

- a scan is never swallowed — a character a scanner sends arriving while a result
  has focus pulls focus back to the field (and clears it) so the reader's
  characters land, while Enter and Space still activate the focused result (Space
  is excluded from the redirect because activating a focused control is a keyboard
  convention a screen reader user relies on);
- a running search keeps focus — the swap replaces the focused result button, so
  focus follows the same product id when it is still a match and returns to the
  field when it is gone, instead of falling to `document.body`;
- the announced state is honest — one polite region describes one state at a time,
  so the previous match count is never announced beside "Searching…", and a failed
  search resets that region to say so instead of leaving "Searching…" forever.

Slice E4 is the redesigned parties screens, in `tests/test_parties.py`:

- the customers and suppliers lists are names-only — the drawer owns the
  badges, ageing, limits, phone and costs — verified with seeded phone, limit
  and cost data that would surface if any of it leaked back into a row;
- clicking a name opens the slide-over drawer, and the test asserts the rendered
  content: the header, the balance line and the seeded Confirmed credit sale for
  the customer; the outstanding balance and the Confirmed purchase for the
  supplier;
- the New customer/New supplier buttons open the create modal, and submitting
  lands the created name in the refreshed list; each row's edit control opens a
  prefilled modal whose saved change lands in the row;
- paying a supplier from the drawer swaps the outstanding balance in place. The
  browser proves the value is displayed, not the arithmetic.

The design screenshots are an opt-in probe, shaped like the harness artifact
probe: `ROYA_E2E_PARTIES_SCREENSHOT_PROBE=1` writes full-page PNGs of the six
screens (customers list, customer drawer, customer create modal, suppliers list,
supplier drawer, supplier edit modal) under `e2e/.artifacts/design/`
(git-ignored) for a human to open.

It deliberately does **not** verify business rules: balances, stock deduction,
payment traceability, numbering and the rest stay in the Rust suite, which is
faster and already thorough. Keeping that boundary sharp is what stops the
browser suite from becoming a slow duplicate.

## Failure artifacts

Tracing is on for every test but the evidence is kept only when a test fails,
under `e2e/.artifacts/<test name>/` (git-ignored):

- `trace.zip` — open it with the trace viewer:

  ```bash
  cd e2e
  uv run playwright show-trace .artifacts/<test name>/trace.zip
  ```

- `screenshot.png` — the page as the test last saw it.
- `server.log` — the application's own output, because "the page did not load"
  is usually answered by the server log.

A green run leaves no artifacts. It does rewrite the Python caches
(`__pycache__` and `.pytest_cache`), which are git-ignored.

To check the artifact pipeline itself, run the opt-in probe. It deliberately
fails its first test, then verifies that the files it left behind form a valid
trace, so the failure is expected:

```bash
cd e2e
ROYA_E2E_ARTIFACT_PROBE=1 uv run pytest tests/test_harness.py -k failure_artifacts
```

## Layout

```
e2e/
  pyproject.toml   the uv project; uv.lock and .python-version pin the environment
  conftest.py      server, database, browser and artifact fixtures
  helpers.py       HTTP-API seeding helpers
  tests/           the browser tests
  README.md        this file
scripts/e2e.sh     the one command
```
