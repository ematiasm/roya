# Query-state derivation advisories (from PR #144)

## Objective

Close the two non-blocking advisories left on record by the receipt-driven
review of PR #144 (`test/sweep-query-param-page-states`), both of which the PR
itself declares as limits:

- `R3-linked-state-derivation-gaps` (WARNING) — the derivation of
  `<route>?<query>` page states reads template `href`s only, so a state offered
  by JavaScript or built at runtime is neither derived nor swept.
- `R3-template-count-guard` (SUGGESTION) — `assert len(_TEMPLATE_SOURCES) >= 60`
  is a floor, and a floor cannot tell a complete derivation from a partial one.

## Problem / Why

The invariants net's whole claim is that a state added tomorrow is swept
tomorrow and a state that leaves the sweep fails loudly. Both advisories name a
place where that claim is currently silent: a template directory layout the
globs stop covering (floor keeps passing at any count), and a link the shell
renders that no template `href` declares (every sweep passes against a shorter
list).

Measured on `main` at `3485114`:

- 74 template files: 22 top-level + 52 in `templates/partials/`; the recursive
  walk and the two explicit globs agree today.
- Exactly **one** query-parameter state exists in the whole app:
  `/settings?tab=taxes`.
- Zero query `href`s in `static/*.js` (`htmx.min.js`, `picker.js`) and zero in
  `src/*.rs`.

So both limits cost nothing today — they are limits, not defects. The work is
to give each one teeth instead of a comment.

## Scope

Authorized: `e2e/tests/test_app_invariants.py` only, plus this document.
Not authorized: templates, `static/`, `src/`, the visual baseline, CI workflows.

## Design decision (user-selected)

The derivation-gap advisory is closed by **verifying against the rendered
shell**, not by extending the derivation sources and not by unioning DOM-observed
states into the sweep:

- Templates stay the single declaration (PR #144's design: derive, never list).
- A new invariant asserts that every internal anchor the shell renders with a
  query string is a state the sweep already visits, and that its path names a
  registered page route (the derivation drops unknown paths silently today).
- When it fires, the message states the limit out loud: make the link one the
  derivation reads, or extend `_linked_page_states` deliberately.

Rejected alternatives: reading `static/*.js` too (adds a source list that today
contributes nothing), and DOM union (makes the browser a requirement of the
derivation and lets template-derived coverage rot without noise).

## Tasks

- [x] **T1 — Replace the template floor with a two-signal equality.**
  New `_assert_every_template_source_was_listed()` mirroring
  `_assert_every_route_call_was_parsed()`: the two explicit globs (what the
  module believes) against `TEMPLATES_DIR.rglob("*.html")` (what is there),
  reported per file, plus a non-empty assertion. Called from
  `_linked_page_states` in place of `len(_TEMPLATE_SOURCES) >= 60`.
  Route: inline (single file, already fully mapped, no open design).
- [x] **T2 — Add invariant 7: every query state the shell renders is swept.**
  New test looping `sweepable_routes`, collecting same-origin `a[href]` with a
  query (resolving relative hrefs against the current page), asserting each
  target is in the swept concrete URLs and its path maps to a registered route.
  Module docstring gains item 7.
  Route: inline (same file).

## Acceptance criteria

- [x] The `>= 60` floor is gone; no magic count remains in the module.
- [x] T1 fails, naming the file, when a template exists outside the two globs
  (mutation: `templates/partials/legacy/x.html`).
- [x] T2 fails, naming the state, when the shell renders a query link the template
  `href`s do not declare (mutation: a JS-built `href` in `base.html` built by
  string concatenation so the regex cannot read it).
- [x] On a clean tree: `scripts/e2e.sh -k app_invariants` green, then the full
  `scripts/e2e.sh` green, `cargo test --locked` unchanged (no Rust touched).
- [x] Tailwind / visual baseline untouched (no template or stylesheet edit).

## Test-first note

These are themselves tests, so there is no runnable RED test to write first.
The equivalent discipline is the mutation proof required above: observe first
that the current suite passes **silently** under each mutation (that is the
defect), then implement, then observe the new assertion catching the same
mutation, then observe green on a clean tree.

## Checks

- `scripts/e2e.sh -k app_invariants` (per task, during iteration)
- `scripts/e2e.sh` (at closure; CI gate)
- `cargo test --locked` (CI gate; no Rust touched, run for evidence)
- `git diff --check`

## Route declaration

Inline direct for both tasks: one non-trivial file, evidence gathered in
bounded batches, design resolved by the user before the first write. No writer
delegation triggered (writer trigger is 2+ non-trivial files).

## Delivery

Forecast ≈ 90; actual 306 authored changed lines — 155 in the test module and
151 here — under the ~400 line budget, so `single-pr`. Push/PR is the user's call.

## Progress

Evidence, in order:

1. **RED (both gaps silent)** — with `templates/partials/legacy/stray.html` in
   place AND a JS-built `href` in `base.html`, `scripts/e2e.sh -k app_invariants`
   → **9 passed**. Nothing caught either defect: the floor cleared at 74
   templates, and no sweep read the state JavaScript offered. Both mutations
   reverted.
2. **T1 fires** — same stray file → `AssertionError: the templates this module
   reads are not the templates that exist: - partials/legacy/stray.html exists
   but is listed by no glob, so its links are never read`, through
   `test_every_page_answers_a_success_status`. Mutation reverted.
3. **T2 fires** — JS-built `/settings?tab=audit` →
   `AssertionError: 1 query state(s) the shell renders are not swept: -
   /settings?tab=audit: / (/) renders <a href='/settings?tab=audit'>: … is a
   state of /settings the sweep never visits: make it a template href the
   derivation reads, or extend _linked_page_states deliberately`. Deduped to one
   line while the link was rendered on every page. Mutation reverted.
4. **Clean tree** — `scripts/e2e.sh -k app_invariants` → **10 passed** (9 before);
   full `scripts/e2e.sh` → **180 passed, 5 skipped** (179 before);  `cargo test --locked` → **1486 passed, 0 failed** (unchanged, no Rust touched);
   `git diff --check` clean. A direct call of the new guard on the clean tree
   reads all 74 templates and `_linked_page_states` still derives exactly
   `['/settings?tab=taxes']`.

Next step: pushed / pull request — the human's decision under ordinary
repository policy.

## Work-unit commit and review assessment

- Work-unit commit: the commit that carries this line, `test(e2e): close the
  two query-state derivation advisories from PR #144` (one commit on this
  branch, tests and this document travel with the behaviour).
- Native assessment of that commit (`gentle-ai review assess --base-ref main
  --committed-only`): risk **`medium`**, reason `executable_change` on
  `e2e/tests/test_app_invariants.py`; `review_due: false`,
  `review_due_reason: under_budget` — 306 authored changed lines against the
  ~400 line slice budget. The slice stays pending: a later commit that reaches
  the budget, or a `high_risk` assessment, is what makes it due. No consent
  envelope was raised and no review authority was consumed.
