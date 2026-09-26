# Rebuild the compiled stylesheet and close the coverage gap it hides

## Objective

Regenerate `static/tailwind.css` so every utility class the templates already use actually has a rule, prove the change is purely additive, and add the durable guard that stops the committed stylesheet from silently going stale again.

## Problem

`static/tailwind.css` is generated and committed. It is stale: fifteen utility classes used by templates that exist have no rule in the compiled file, so those elements render unstyled today.

A prior record blamed an older Tailwind version. **That was false and is corrected here.** The committed file's header and the installed CLI are both `v4.3.3`. The file is not behind the tool; it is behind the templates.

Two facts make this more than a rebuild:

1. **The rebuild alone would be unproven on all of its own surface.** The visual baseline visits neither `/settings` nor `/setup`, and **all fifteen** newly styled classes live on exactly those two pages: ten on the Settings Taxes tab and the locale profiles (`self-end`, `whitespace-nowrap`, `sm:grid-cols-[100px_1fr_120px_auto]`, `max-w-2xl`, `w-auto`, `rounded-xl`, `justify-end`, `md:grid-cols-2`, `md:grid-cols-[1fr_auto]`, `md:items-end`, `md:mb-0`) and five on the first-run setup wizard (`max-w-2xl`, `min-h-[70vh]`, `mt-8`, `pt-5`, `sm:grid-cols-2`). The baseline stayed green through the rebuild, and that green is worthless: the net covered **0 of 15**. This is the "a net is only as wide as the pages it visits" failure this repository has already been bitten by, and it is why U2 exists.
2. **Nothing prevents recurrence.** There is no test that a class used by a template has a rule in the committed stylesheet. The stylesheet went stale silently, and it will again.

## Why

- A committed generated artifact is a promise: the app must render correctly from a clean checkout with no CLI installed. Fifteen missing rules break that promise on a real screen, not in theory.
- A rebuild that is not re-stale-able is a one-time fix. The guard is the actual deliverable.

## Decisions

- **Rebuild with the committed command.** `scripts/build-css.sh` is the canonical build, not a hand invocation. Same tool, same input, same flags as any prior rebuild.
- **The change must be purely additive.** A regeneration that removes or modifies a single existing rule is a red flag, not a fact of life, and must be investigated before the file is committed. The measurement is the rule-level diff, not the byte count.
- **The guard is a real test with a real RED.** A test that a class used by a template exists in the compiled stylesheet will be written first, observed failing against today's stale file, and only then satisfied by the rebuild. It must parse class attributes, not prose, and must be escape-aware for arbitrary values and variants, or it will report false positives.
- **Close the baseline coverage gap in the same work.** `/settings` and `/settings?tab=taxes` are added to the visual baseline, with the failing run proving the gap first. That is two recorded follow-ups closed at once.
- **Honest accounting of what the baseline does and does not cover.** The three Settings classes are proven by the baseline once the Settings pages are captured; until then they are only proven by the rule's existence in the compiled file. Both facts get recorded, not conflated.
- Technical artifacts, code, comments, tests, and UI copy are English.

## Authorized scope

Repository-local stylesheet, guard test, e2e harness, and feature-document changes. No push, PR creation, or merge without explicit user request. No database reset, no change to the development database, and no change to `assets/tailwind.css` — the input is already correct and must not be edited to make the output look different.

## Route declaration

Two bounded delegated-direct ODD work units, one writer each: the rebuild and its guard, then the baseline coverage extension. The parent owns task closure, work-unit commits, verification, and delivery reporting. No SDD phase is created.

## TDD mode

- Effective mode: strict TDD inherited from project configuration (`openspec/config.yaml`, `strict_tdd: true`).
- Test runner: `cargo test`, plus the Playwright harness for the baseline.
- The guard's RED is observed against the stale stylesheet, before the rebuild.

## Work units

- [x] U1 — Rebuild the stylesheet and add the staleness guard.
  - The guard was written first and observed RED against the stale stylesheet, naming 15 of 240 class tokens with no rule, each attributed to the templates that use it. GREEN after the rebuild: 247 class names defined, 240 tokens checked, 0 missing.
  - Rebuilt with `scripts/build-css.sh`; `assets/tailwind.css` untouched and a second build byte-identical.
  - The rule-level diff is purely additive: thirteen character-level insertion runs, zero deletions and zero replacements, 679 characters inserted, fifteen selectors added and zero removed. One rule body grew by exactly one declaration, the `--container-2xl:42rem` token `max-w-2xl` consumes.
  - Every one of the fifteen added rules is enumerated below and each corresponds to a class a template actually uses; the guard's RED set and the rebuild's added set are equal.
  - The guard's parsing is anchored, never lexical, and escape-aware by parsing the compiled stylesheet and unescaping per the CSS escape rules rather than substring-matching or hand-escaping. It reads class attribute values, the two `@source` paths the entrypoint declares, `className` assignments and `classList` string arguments, skipping Askama control blocks atomically and stripping comments on both sides.
  - Every behavioural claim in the guard is mutation-proven, not asserted: a class removed from the compiled file fails it, a class added to a template fails it, a removed scan path fails it, and a silently-empty extractor fails it on floors and sentinels. Two real defects were found and fixed this way — a single-argument `classList.toggle` dropping its class, and a method merely *starting* with a writer name (`toggleAll`) being read as `toggle`, which both faked a class and swallowed every call after it.
  - Evidence: `cargo test stylesheet` → 10 passed; `cargo test` → 1117 passed; `cargo check --all-targets` → 0 errors with 0 warnings attributed to the new module; `cargo fmt --check` and `git diff --check` clean; `bash scripts/e2e.sh tests/test_visual_baseline.py` → 1 passed, `tests/test_settings.py` → 6 passed, `tests/test_products.py` → 24 passed / 1 pre-existing opt-in skip. Parent spot check repeated `cargo test stylesheet` → 10 passed. Commit identity is recorded in this document after the work-unit commit.

- [ ] U2 — Close the baseline coverage gap for the two unbaselined pages.
  - **The plan as first written was wrong and is corrected here.** It covered only `/settings` and `/settings?tab=taxes`, but five of the fifteen classes (`max-w-2xl`, `min-h-[70vh]`, `mt-8`, `pt-5`, `sm:grid-cols-2`) live exclusively on `/setup`, so U2 as originally scoped would have left them uncovered. U2 must cover the first-run setup wizard too, otherwise the net still misses part of the surface this feature just styled.
  - Add `/settings`, `/settings?tab=taxes` and the setup wizard to the visual baseline, and observe the failing run that proves the gap first.
  - Regenerate the baseline only after confirming the failure is exactly the intended correction, per the discipline `odd/tasks/ui-component-tokens.md` already records: a baseline regenerated to turn a red test green is not evidence.
  - Confirm the fifteen newly styled classes are now **covered**, not merely present in the stylesheet.
  - Evidence: failing run, then passing run, exact commands, commit identity.

## Acceptance criteria

1. Every utility class used by a scanned source has a rule in the committed `static/tailwind.css`.
2. A guard test fails when a template uses a class the committed stylesheet lacks.
3. The guard is escape-aware for arbitrary values and variants and produces no false positive on the current tree.
4. The rebuild removes no existing rule and modifies no existing declaration.
5. Every added rule is enumerated and each corresponds to a class a template actually uses.
6. `/settings`, `/settings?tab=taxes` and the setup wizard are covered by the visual baseline, so all fifteen newly styled classes are covered rather than merely present.
7. The visual baseline is regenerated only after a failing run proves the failure is the intended correction.
8. The false "older Tailwind" record is corrected everywhere it appears, without rewriting unrelated history.
9. `assets/tailwind.css` is unchanged.
10. Focused tests, the full Rust suite, and the browser checks pass, with all skips recorded.

## Applicable checks

- `cargo test` (the staleness guard runs in it)
- `cargo test stylesheet`
- `cargo check --all-targets`
- `cargo fmt --check`
- `bash scripts/e2e.sh tests/test_visual_baseline.py`
- `bash scripts/e2e.sh tests/test_settings.py`
- `bash scripts/e2e.sh tests/test_products.py`
- `git diff --check`

## Progress and evidence

- Baseline: branch `feat/tax-calculation-settings` at `e7839cd`, working tree clean apart from the unrelated untracked `odd/tasks/pos-counter-sales.md`, which must never be staged by this task.
- Measured before any change: installed CLI is `tailwindcss v4.3.3`; the committed stylesheet's header is `tailwindcss v4.3.3`. Same tool.
- **Corrected 2026-09-26 (U1).** The pre-change probe recorded here was wrong in three ways; all three are corrected below against the real build. What survives: 27,876 → 28,555 bytes, and 44 top-level blocks in both files (verified on `HEAD` and on the rebuilt file).
  - The character-level diff is **thirteen insertion runs, zero deletions and zero replacements** (679 characters inserted) — not twelve. Measured with `difflib.SequenceMatcher(HEAD, rebuilt, autojunk=False)` over the two file contents and counting the non-`equal` opcodes by kind.
  - **Fifteen selectors are added and zero removed.** The old text said "zero selectors added and zero removed", which contradicted the fifteen inserted rules it listed in the same sentence. Measured at selector level: the parsed rule set grows by exactly fifteen, with 0 selectors removed. (The absolute rule count depends on whether the parse counts the top-level `@layer`/`@supports` blocks as rules, so only the delta and the 15/0 split are stated here.)
  - The class is `md:mb-0`, not `mb-0`. No template uses bare `mb-0`; the only occurrence is `md:mb-0` (`templates/settings.html:111`), and the rebuild adds `.md\:mb-0` inside `@media (min-width:48rem)`. `mb-0` is absent from the committed stylesheet too, but nothing asks for it.
- The fifteen added rules, one per class a template actually uses: `justify-end`, `max-w-2xl`, `min-h-[70vh]`, `mt-8`, `pt-5`, `rounded-xl`, `self-end`, `w-auto`, `whitespace-nowrap`, `sm:grid-cols-2`, `sm:grid-cols-[100px_1fr_120px_auto]`, `md:grid-cols-2`, `md:grid-cols-[1fr_auto]`, `md:items-end`, `md:mb-0` — plus the `--container-2xl:42rem` theme token that `max-w-2xl` consumes.
- The fifteen are exactly the guard's RED set: the guard named fifteen missing classes before the rebuild, and the rebuild adds fifteen rules. No rule without a user, no user without a rule.
- `assets/tailwind.css` is unchanged, and a second `scripts/build-css.sh` is byte-identical to the first.
- **The visual baseline stayed green through the rebuild, and that green is worthless.** Every page it visits was already fully styled, so none of the fifteen newly styled classes appears in any capture. Measured, not predicted: its `goto` list contains no `settings` and no `setup`, and all fifteen classes live on exactly those two pages. A green run over an unvisited page cannot be distinguished from a correct one — this is the "a net is only as wide as the pages it visits" lesson, and it is what U2 exists to close.
- Feature document: `odd/tasks/tailwind-stylesheet-rebuild.md`.
- Engram mirror topic: `odd/tailwind-stylesheet-rebuild/tasks`.
- U1 delivery: no commit recorded yet.
- Next step: U2, the baseline coverage extension for `/settings`, `/settings?tab=taxes` and the setup wizard.

## Known gaps

- **Inline scripts in templates are outside the guard's net.** The guard reads HTML as `class="…"` attributes only, so the **27 inline `className` / `classList` call sites in the templates' `<script>` blocks** are unmonitored: `base.html` (3), and `customers.html`, `documents.html`, `products.html`, `purchase.html`, `purchases.html`, `suppliers.html` (4 each). All 27 are inside a `<script>` block, and between them they write **six** distinct class names — `-translate-x-full`, `flex`, `hidden`, `notice`, `notice-error` and `notice-success` — all six of which have rules in the committed stylesheet. The first three come from the `classList` calls and the notice trio from `box.className` in `base.html`. The direction of the blindness is safe: Tailwind scans those script bodies as raw text, so the compiler's coverage there is a **superset** of the guard's, and the guard can only under-report — never false-positive, and never hide a class the compiler successfully compiled. What it costs is that the guard's coverage claim is scoped to class attributes and to the JavaScript scan sources. Covering inline scripts means parsing JavaScript inside HTML — a second parser to keep correct — and is deliberately out of scope for this unit. Recorded in the module documentation of `src/stylesheet_tests.rs` beside the code.
- **`classList.toggle('a', 'b')` would still drop `'b'`.** `toggle` drops its trailing argument when more than one argument is present, to skip the optional `force` flag. That is right for every call in this repository, but two class names with no flag would lose the second one. No such call exists; a fix would key on whether the trailing argument is a string literal rather than on the count.
- **`{{ … }}` inside a class attribute is a hard failure, not a skip.** There are none today. If one is ever added the guard refuses to pass rather than going blind, which is the safe direction but does mean the failure is a test error until someone extends the extractor.
- **A file type with no extractor in a scanned path panics.** Deliberate: loud beats silently uncovered.
- **`width`, `height` and the vertical margins are not in the visual baseline's property set**, by that suite's own documented decision (they resolve from font metrics). A dropped `w-full` would not be caught there. Pre-existing, from `ui-component-tokens`, not introduced here.
