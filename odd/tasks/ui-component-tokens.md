# Feature: UI component tokens — move the styling decision out of the element default

## Objective

Empty `@layer base` of its element opinions and express the interface's real
components as component classes, so that styling a control is a decision made
where it is used rather than a fight against a default. The visible result should
be **nothing**: this is a refactor whose success criterion is that no screen
changes.

## Problem

Read from the tree on 2026-09-24, after PR #98.

`assets/tailwind.css` styles the bare elements in `@layer base`:

```css
a      { @apply text-accent2 no-underline hover:underline; }
button { @apply ... bg-accent ... font-bold text-[#0a0f0d]; }
label  { @apply block text-[13px] font-semibold tracking-[0.04em] text-muted uppercase; }
input,
select,
textarea { @apply w-full rounded-[10px] border border-border bg-bg ...; }
```

So every `<a>` in the system is blue, every `<button>` is a mint primary button,
every `<label>` is uppercase grey, and every input is full width. Every screen
that wants something else has to override that, and the measurements say most of
them do:

| Element | Total | Overrides the default |
|---|---|---|
| `<button>` | 118 | **73** (62%) |
| `<a>` | 29 | **14** (48%) |
| `<input>` | — | **64** with a forced width |

**When 62% of buttons override the button style, that style is not a default — it
is an exception that was promoted to one.** And because there is no component,
every author re-types the override: `rounded-[10px] border border-border
bg-transparent px-2.5 py-1.5 text-xs text-text` appears **61 times, verbatim**,
in five slightly different variants across 12+ templates.

The two costs:

1. **Inconsistency by construction.** Changing the secondary button's radius
   means editing 61 places. Miss one and that screen is subtly different, which
   is what "less aesthetic" looks like from the operator's chair.
2. **Structural collisions.** `a { text-accent2 }` leaks into anything that
   becomes an anchor. The repository has already paid for this three times, and
   its own commit messages say so:
   - `ac49610` — *"the row became an anchor so it could open the peek, the
     identifier, the supplier and the meta line all inherited that blue"*.
   - `c6537d9` — *"the ten buttons carrying `bg-accent2 text-white` were not a
     second palette — they were **overrides fighting the base style**"*.
   - `f1e27e7` — *"this is not a new colour choice; it **removes an override
     that was fighting the base style**"*.

   Each time the fix was to strip the offending classes from that screen, leaving
   the default in place waiting for the next one. This feature removes the
   default instead.

**The measured pattern inventory** (the design is derived from these, not from
imagination):

| Component | Occurrences | The string being repeated |
|---|---|---|
| secondary button | **61** | `rounded-[10px] border border-border bg-transparent px-2.5 py-1.5 text-xs text-text` |
| chip | **37** | `rounded-full border px-2 py-0.5 text-[11px] font-bold tracking-[0.04em] uppercase` + a colour |
| empty state | **19** | `rounded-xl border border-dashed border-border ... text-[13px] text-muted` |
| notice box | **9** | `rounded-xl border border-{accent,danger}/40 bg-{accent,danger}/10 px-4 py-3 text-sm text-{accent,danger}` |
| danger button | **6** | `rounded-[10px] border border-danger/30 bg-danger/12 ... text-danger` |
| panel | **3+** | `rounded-xl border border-border bg-card p-6` |
| borderless row-name button | **3** | `bg-transparent p-0 text-left font-semibold` |
| menu item | **2** | `block w-full rounded-[10px] bg-transparent px-3 py-2 text-left text-sm` |

## Why

Three reasons, in order of weight:

1. **It removes a defect class rather than a defect.** The anchor-blue row cannot
   happen again if the anchor has no colour. Two previous fixes were symptoms.
2. **The notice boxes are duplicated in three places on purpose.** `notice.html`,
   `purchase_merge_notice.html` and the `notice()` builder in `base.html` carry
   the same class list, and the repository's own comment says *"Keep the three
   copies in step"*. With a `.notice` component the three copies become one
   class, and the instruction to keep them in step stops being needed.
3. **It makes the next piece of work readable.** The wide-screen layout work
   touches the same rows. Doing it first would mean every layout diff arrives
   with 79 characters of classes attached.

## Scope

- `assets/tailwind.css` (the components) and the regenerated `static/tailwind.css`
- The templates that carry the patterns above — ~20 files
- `templates/base.html`'s `notice()` JS builder (its class list becomes the
  component)
- `static/picker.js`, which builds the results rows and the notice shape from JS

Out of scope: any change to what a screen looks like, the layout/width work, the
`@theme` palette (it is fine as it is), and the `label`/`input` defaults — those
are addressed in the design decisions below, not silently dropped.

## Constraints

- **Visual neutrality is the acceptance criterion.** If a screen changes, the
  refactor failed, even if it looks better.
- **Tailwind `source(none)`**: `assets/tailwind.css` declares
  `@source "../templates"` and `@source "../static/picker.js"`. A class that
  exists only in a file outside those trees is purged.
- **The compiled stylesheet is committed** and must be regenerated, not
  hand-edited.
- **The three notice copies must end up identical**, or the component has not
  done its job.
- **`@layer components` beats `@layer base` by layer order**, verified in the
  compiled output: `properties < theme < base < components < utilities`. So a
  component class overrides an element default **without `!important`** and
  without specificity games. This is the mechanism the refactor relies on.

## Locked design decisions

1. **`@layer base` keeps only what is universal**: `body`, and `dialog` (whose
   comment explains a real preflight collision). The `a`, `button`, `label` and
   `input`/`select`/`textarea` rules are removed.
2. **A control with no component class carries no styling.** That is the point:
   an unstyled control is a visible mistake, not a silent wrong colour.
3. **Component set**, named after what they are and tied to the existing semantic
   tokens:
   - `.btn-primary` — the mint button, exactly the current `button` default.
   - `.btn-secondary` — the bordered transparent button (61 occurrences).
   - `.btn-danger` — the destructive variant (6).
   - `.btn-plain` — the borderless button that names a row (3). Its `p-0` and
     `text-left` are part of it; the caller keeps only layout classes.
   - `.chip` plus `.chip-income`, `.chip-expense`, `.chip-warning`, `.chip-muted`
     (37). The variant names come from the palette tokens so the semantic link
     is visible at the call site.
   - `.notice` plus `.notice-success`, `.notice-error` (9), used by all three
     copies.
   - `.empty` — the dashed empty-state box (19).
   - `.card` — the bordered panel (3+).
   - `.menu-item` — the dropdown row (2).
   - `.link` — the prose link, exactly the current `a` default.
4. **`label` and `input` lose their defaults too.** `label`'s uppercase grey is
   right for forms and wrong for anything else, and `input`'s `w-full` is wrong
   for the many narrow numeric fields. They become `.field-label` and `.field`,
   with the call sites naming them. **This is the largest mechanical part of the
   change and the one most likely to be under-estimated**, so it is its own task.
5. **No `!important`, no specificity escalation.** If a component does not win,
   the layer order is being used wrong.
6. **The components live in `@layer components`**, alongside the existing
   `.htmx-indicator` pair.

## Acceptance criteria

- [ ] `@layer base` no longer styles `a`, `button`, `label` or
      `input`/`select`/`textarea`.
- [ ] Every `<button>` and `<a>` in the templates carries a component class, or
      is deliberately unstyled with a comment saying why.
- [ ] The three notice copies carry the same class, and the
      "keep the three copies in step" instruction is no longer needed.
- [ ] The 61-occurrence secondary-button string appears **zero** times.
- [ ] **No screen changes.** Proven by a computed-style comparison, not by
      inspection.
- [ ] The compiled stylesheet is regenerated and committed.
- [ ] `cargo test` green; `scripts/e2e.sh` green; CI green.

## Applicable checks

- `cargo test` (primary; `openspec/config.yaml` sets `strict_tdd: true`)
- `scripts/e2e.sh` — the behavioural suite, which must stay at 100 passed
- `scripts/build-css.sh` plus a purge assertion for any class that lives only in
  `static/picker.js`
- The computed-style comparison (below)
- CI, once the branch is pushed

## TDD

`strict_tdd: true`, and this is the rare refactor where the honest red is a
**snapshot**: capture the computed styles of a fixed set of elements across a
fixed set of pages *before* touching the CSS, commit that as the golden file,
and let the refactor be red until the rendering matches again.

The snapshot must be taken on the pre-refactor tree. A snapshot taken afterwards
would record whatever the refactor produced and prove nothing.

## Tasks

- [x] T1 — **The visual-neutrality net.** Closed in `97c04de`. A browser test
      fingerprints every element's computed style on thirteen screens and
      compares it to a committed baseline: 2,341 elements, 36 KB compressed,
      deterministic across consecutive runs. Elements are keyed by **DOM path,
      not by class** — the classes are what the refactor rewrites, so keying by
      them would make every entry look changed and the comparison useless — and
      the baseline was captured from the **pre-refactor tree**, with regeneration
      behind an environment variable CI never sets.
      **Proven by mutation, because a net that catches nothing is the vacuous
      test problem in a new costume**: changing `--color-accent` from `#6ee7b7`
      to `#6ee7b8`, one digit and invisible to the eye, makes it fail with a
      readable diff, and it catches the derived `oklab` values of the `/10`
      opacity variants too. Reverted and verified byte-identical afterwards.
      The two drawers are reached by clicking rather than by URL: they are
      fragments, and navigating straight to `/products/detail/{id}` snapshots
      three elements and proves nothing.
- [ ] T2 — **The button and link components.** `.btn-primary`,
      `.btn-secondary`, `.btn-danger`, `.btn-plain`, `.menu-item`, `.link`; the
      `a` and `button` base rules removed. The 61 secondary occurrences replaced.
      Largest single win, and the one that kills the anchor-blue class.
- [ ] T3 — **The chip component.** `.chip` plus its four variants, 37
      occurrences.
- [ ] T4 — **The boxes.** `.notice` (all three copies), `.notice-success`,
      `.notice-error`, `.empty`, `.card`.
- [ ] T5 — **The form controls.** `label` → `.field-label`, `input` → `.field`,
      across the templates. Deliberately last: it is the widest mechanical change
      and the one most likely to hide a visual difference, so it happens with the
      net already in place and everything else stable.

## Delivery strategy

Five tasks, and each is independently revertible because the net lands first.
T2 is the highest-value slice and could ship alone; T5 is the riskiest and ships
last.

## Progress

- 2026-09-24: document created. The pattern inventory was measured from the
  templates rather than guessed, which changed the design: the chip turned out to
  be as large as the button (37 occurrences), and the notice duplication across
  three files turned out to be a bigger win than the class-list length alone
  suggested.
- 2026-09-24: T1 closed in `97c04de`. The net is in place and proven by mutation
  before a single class was touched, which is the only order that works: a
  baseline captured after the refactor records the refactor and proves nothing.

## Verification evidence

- **T1, the net catches a real change**: `--color-accent` `#6ee7b7` → `#6ee7b8`
  (one digit, invisible to the eye) failed with
  `color: 'rgb(110, 231, 183)' -> 'rgb(110, 231, 184)'` and the same for
  `border-top-color` and `background-color`, plus
  `background-color: 'oklab(0.845178 -0.125467 0.0336939 / 0.1)' ->
  'oklab(0.845427 -0.125022 0.0324349 / 0.1)'` — so a token change propagates
  into the alpha-composited colours where reading would never find it. Reverted
  and both CSS files verified byte-identical.
- **T1, determinism checked rather than assumed**: two consecutive runs both
  pass in the same 9.03s. A flaky net would be worse than none, because it
  would teach people to re-run until green.
- **T1, size**: 1,391,684 bytes uncompressed, **36 KB gzipped**, which is what
  lands in the repository — smaller than the committed `Cargo.lock`.
- **T1, green (orchestrator-verified)**: `cargo test` → **885 passed,
  0 failed**; `scripts/e2e.sh` → **101 passed, 4 skipped, 0 failed** (100 plus
  the net).
- **T1, the message outlives the refactor**: it says that an unintended change
  means the diff is the defect, an intended one means regenerating deliberately
  and saying so in the commit, and that a baseline regenerated to make a red
  test green is not evidence.
