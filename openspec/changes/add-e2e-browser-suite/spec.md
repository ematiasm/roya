# Spec: add-e2e-browser-suite

## Requirements

### R1 — No Node toolchain
The suite runs under Python managed by `uv`. It must not require `package.json`, `node_modules` or `npx`,
and the repository must not gain a Node toolchain. The reason is recorded in the proposal: Tailwind is
built from its standalone binary precisely so that `cargo run` needs no package manager.

### R2 — The real binary against a throwaway database
The suite builds the application, starts the real binary against a temporary SQLite file on a free port,
waits until it answers, and tears it down when the run ends. The development database must never be
touched by a test run.

### R3 — Data created through the API
Fixture data is created by calling the same HTTP endpoints a user's browser calls, so the seed cannot
depend on schema internals and cannot drift from the interface.

### R4 — Failures leave evidence
A failing test writes a Playwright trace and a screenshot under an ignored artifacts directory, so a
human can open the trace and see what the browser saw.

### R5 — Selectors pinned to what the interface already exposes
Tests address the interface through the ids it already has: the picker wrapper, the picker input, the
results container and the results status. Where a stable hook is genuinely missing, one is added
deliberately and documented; tests must not depend on incidental copy or on DOM position.

### R6 — One documented command
A single documented command runs the suite. The Rust suite stays independent of it and keeps its current
speed.

## Acceptance criteria
- [ ] AC1: one documented command runs the suite, and it needs no network access once the browser is
      installed.
- [ ] AC2: the suite starts the real binary and leaves the development database byte-identical, with the
      port released on exit.
- [ ] AC3: scanning a barcode adds the line in a single interaction, and the picker comes back empty and
      focused.
- [ ] AC4: choosing a product from the results adds it with the quantity typed in the picker, so the two
      interactions are distinguished rather than conflated.
- [ ] AC5: filtering the sales list **updates the URL**, and reloading that URL preserves the filter.
- [ ] AC6: the arrow keys move the highlight through the results and Enter adds the highlighted product.
- [ ] AC7: while a search is in flight the interface shows that it is working, rather than an empty area.
- [ ] AC8: cancelling a sale asks for confirmation; dismissing it leaves the sale untouched, and
      accepting it cancels the sale.
- [ ] AC9: the results announce the match count to assistive technology.
- [ ] AC10: a deliberately failing test writes a trace that opens with the trace viewer.
- [ ] AC11: no fixed sleeps in the suite; two consecutive runs produce the same result.
- [ ] AC12: `cargo test` still passes unchanged, and the e2e suite neither slows it down nor depends on it.

## Notes on AC5, AC6 and AC7
These three describe behaviour that **does not exist yet**, and that is their purpose: they are written
first and expected to fail, so the suite proves it can see the defects that motivated it. They are the
acceptance criteria of slice E3, while AC1 to AC4 and AC8 to AC12 belong to E1 and E2.
