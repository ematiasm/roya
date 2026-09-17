# Proposal: add-e2e-browser-suite

## Workflow
ODD with OpenSpec artifacts. Slices sized for review, independent verification per slice, archive on
merge. This change adds a second test toolchain, which is a deliberate exception to the project's
one-language rule and needs the reasoning below to stand on its own.

## Problem statement
The current suite verifies the server: routes, guards, derived state, error mapping, and the rendered
HTML's wiring. It cannot verify **behaviour in a browser**, and that gap is no longer theoretical.

Three defects in the freshly built product search are invisible to every existing test:
1. **Filtering never updates the URL.** No template carries `hx-push-url`, so a filtered list cannot be
   reloaded, bookmarked or shared, and the back button does not undo a filter change. The server accepts
   the parameters and the tests pass, because the tests call the endpoint directly.
2. **The results cannot be traversed with the keyboard.** Matches appear, but reaching one means tabbing
   to each button or using a mouse. In a flow designed for a scanner, that is the difference between fast
   and clunky.
3. **There is no loading state.** During the 250 ms debounce the results area is simply empty, so a search
   in progress looks like a search that found nothing.

All three are properties of htmx running in a browser. The wiring guard checks that a target exists and
that a route resolves, and all three pass that examination. Two earlier bugs were also found only by
driving the interface, which is why the HTTP smoke suite exists; this change extends the same idea to the
layer the smoke suite cannot reach.

## Goal
A small browser suite that drives the real interface, so a user-experience change can be made with a
safety net instead of by reading templates and hoping.

## Tooling decision
**Python with `uv`, using Playwright.** Decided before this change was written and recorded in memory;
this proposal records the reasons so they survive.

- The repository has **no Node toolchain**, deliberately: Tailwind is built from its standalone binary and
  assets are vendored, so `cargo run` works with no package manager. Adding Playwright for Node would
  reintroduce exactly what was avoided. A stray `package-lock.json` from an `npx` invocation already had
  to be removed once.
- Playwright is available for Python with the same API and the same auto-waiting, so the ergonomics that
  make it the right tool are kept without the toolchain cost.
- `uv` manages the Python environment in the repo (`.venv`) without touching the system interpreter.
- Chromium is a one-time dev download of roughly 150 MB, outside the repository, and is not needed to run
  the application.

## Scope

### E1 — the harness
A server fixture that builds and starts the real binary against a throwaway database on a free port,
waits until it is listening, and tears it down; a seeding helper that creates its data **through the HTTP
API** so the seed uses the same paths a user does; a browser fixture; and failure artifacts (trace,
screenshot) a human can open to see what happened.

### E2 — the critical flows
The picker: scanning adds a line in one step, choosing from the results carries the quantity, the field
comes back empty and focused. The filters: each criterion narrows the list, and the resulting URL can be
reloaded and keeps the filter. Confirmation gates a destructive action. Plus the accessibility fix: the
results announce a match count.

### E3 — the three defects, closed
The three defects above become failing tests first, then get fixed: the URL carries the filter, the
results are traversable with the arrow keys, and searching shows that it is working.

## Non-goals
- No attempt at full interface coverage. This suite covers the flows where a browser is the only honest
  judge, and leaves everything provable by HTTP to the Rust suite, which is faster.
- No visual regression screenshots, no cross-browser matrix, no mobile viewport sweep.
- No CI wiring yet, and no change to how the application is built or run.
- The counter mode, ticket printing and PWA remain parked as recorded in the interface redesign.

## Impact
A new `e2e/` directory with its own Python environment, ignored by git except its source; a documented
command to run the suite; and the `.gitignore` entries that already exist for it. The Rust suite and the
application are untouched by the harness itself. E3 does touch templates, which is the point.

## Acceptance summary
One command runs the suite; it needs no Node; it starts the real binary on a throwaway database and
cleans up; a failure leaves a trace a human can open; the picker, the filters and the confirmations are
exercised as a user would; and the three defects are proven closed by tests that failed before.
