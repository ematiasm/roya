# Feature: Purchases — auto-created draft, inline header, supplier picker

## Objective

Make starting a purchase a single keystroke and make the draft's identity fields
editable where the work happens. The operator presses one button, the draft
exists, and supplier / date / invoice / notes are editable on the same page that
loads the products — with the supplier field searchable so it still works when
the supplier list grows.

## Problem

Read from the tree on 2026-09-23, after `redesign-purchases-index` merged:

1. **The primary action's colour was the last blue thing.** The page action in
   the shared `partials/page_header.html:29` was `bg-accent2` (`#60a5fa`). The
   rest of the system speaks the mint accent (`--color-accent`, the `roya ◆`
   logo, the active nav entry, the success notice) — **and the base `button`
   element style is already mint with a dark label**, so the blue buttons are
   overrides fighting the base style, not a second palette. Verified count after
   T1: **ten** templates still carry an explicit `bg-accent2 text-white` —
   `dashboard.html` (Add transaction), `customer_detail.html` (Collect),
   `product_detail.html` (Save product), `supplier_detail.html` (Pay supplier),
   `purchase_detail.html` (Confirm ×2, Record payment ×2) and `sale_detail.html`
   (Confirm, Record payment). Note that the record page's **Add line** is *not*
   one of them: it is a bare `<button type="submit">` and therefore already
   renders mint through the base element rule. Those ten are a decision the user
   still owes, and the base-style fact is the argument: retiring the override
   makes every action one colour without inventing one.
2. **Starting a purchase costs a page.** `New purchase` navigates to
   `/purchases/new`, a four-field page, whose only required fields are the
   supplier and the date. Two of its four fields are the two the operator will
   want to adjust anyway once the goods are in front of them.
3. **The identity fields are behind a dialog.** On the record page the supplier,
   date, invoice and notes are read-only text (`partials/purchase_detail.html`
   around line 205), and the only way to change them is the **Edit header**
   dialog behind the `⋯` menu — and the dialog cannot change the supplier at all
   (`UpdatePurchaseHeaderForm` has no `supplier_id`, though the domain's
   `UpdatePurchaseDraft.supplier_id` does and `update_draft` persists it).
4. **There is no supplier search.** `GET /web/product-search` exists and the
   product picker is built on it, but nothing equivalent exists for suppliers, so
   a supplier is always chosen from a full `<select>`.
5. **The purchase list row is blue — a regression the previous feature shipped.**
   `assets/tailwind.css` gives every anchor `text-accent2` by default
   (`a { @apply text-accent2 … }`). When the previous feature made the whole row
   an anchor so it could open the peek, the identifier, the supplier and the meta
   line inherited that blue; only the total carried an explicit `text-text`.
   Products and suppliers do not have this because their rows are a `<div>` with
   a `<button>` that names `text-text` explicitly. The colour check that shipped
   with that change tested the total's class, not the row's rendered colour, so
   it passed while the row read blue.

## Why

User request, 2026-09-23: the blue button clashes; that same button should create
a draft automatically and land somewhere where supplier, invoice and date are all
editable, on the page that loads the products. Decisions taken with the user:

- **The green is the mint accent, not the money green.** `--color-income` is the
  state colour (`Paid`, `active`) that the previous feature spent effort
  separating from action colours; reusing it for a button would undo that.
- **The colour change is global**, in the shared header component, because the
  same blue is the primary action on seven pages.
- **The default supplier is the last used one, and the field is searchable** —
  the user's reason: it stays useful when the supplier list grows.
- **`/purchases/new` is deleted.** The auto-created draft replaces it; two doors
  to create a draft is the duplication the previous feature removed elsewhere.

## The hazard this design has to respect

`purchases.supplier_id` is `NOT NULL` (`migrations/20240101000033:176`), so
"create a draft automatically" needs a supplier from somewhere. The supplier is
what resolves each line's default cost (the previous feature's fix: the
supplier's satellite cost, falling back to the product column). So a draft born
with the **wrong** supplier and loaded without the operator noticing means:

1. every line records the wrong supplier's cost,
2. confirming writes those costs into that supplier's satellite, overwriting the
   real price,
3. and the purchase is attributed to the wrong supplier.

That is the same defect class the previous feature fixed, triggered by a silent
default. **The mitigation is that the supplier is never silently guessed: the
dialog shows the last-used supplier as a real, visible, editable value, and
choosing is what creates the draft.**

## Authorized scope

- `templates/partials/page_header.html` — the action colour, and an optional
  dialog-opening action mode.
- `assets/tailwind.css` + regenerated `static/tailwind.css`.
- `templates/purchases.html`, `templates/purchase_new.html` (deleted),
  `templates/purchase.html`, `templates/partials/purchase_detail.html`.
- New: `templates/partials/supplier_picker.html` (or a macro in an existing
  partial) and a results fragment.
- `src/routes/purchases_web.rs`, `src/routes/suppliers_web.rs` (the search
  endpoint's home), `src/repositories/purchase_repo.rs` (last-used supplier),
  `src/models.rs` (`UpdatePurchaseHeaderForm` gains the supplier), `src/smoke_tests.rs`.
- `e2e/tests/test_purchases.py`, `e2e/tests/test_picker.py` where the flow changes.
- `README.md`.

## Locked design decisions

1. **Mint, globally.** `bg-accent2 text-white` becomes `bg-accent` with a dark
   text colour. Neither green survives white text: mint `#6ee7b7` on white is
   about 1.4:1, on `--color-bg` about 13:1. The change therefore also fixes the
   contrast the blue button never had.
2. **The supplier field is a text input with a results list, and the name
   resolves server-side.** `suppliers.name` is UNIQUE, so resolving an exact
   name to an id is well defined — the same contract `resolve_product_ref` gives
   the product picker (exact match, then a clear refusal). The results list is
   click-to-submit shortcuts that carry the id, so the widget needs no JavaScript.
3. **One picker, two hosts.** The creation dialog and the record page's inline
   header use the same widget, the way the product picker is shared.
4. **The inline header posts to the existing route.** `POST /web/purchases/{id}/header`
   already exists; it gains the supplier field. The Edit header dialog and its
   `⋯` entry are deleted, because the fields are now in front of the operator.
5. **`New purchase` creates the draft.** Pressing it opens the dialog; choosing a
   supplier (or accepting the pre-filled last-used one) posts to the existing
   `POST /web/purchases` and lands on the new draft's record page.
6. **`/purchases/new` is deleted**, with its route, template, guard entry, tests
   and README mention.

## Out of scope, parked deliberately

- **A nullable `supplier_id`.** It would allow a truly empty draft filled in
  later, but it needs a migration, a confirm-time refusal for a supplier-less
  purchase, and a new case in the cost rule ("no supplier → the product column").
  The user chose the default-plus-search instead.
- **The sales mirror.** Sales keeps its own creation page and its `⋯` Edit
  header dialog.
- Everything the previous feature parked: the filter bar, the unified `q`
  search, the localization feature.

## Constraints

- Reuse existing patterns only: the product picker's htmx shape (debounced
  `hx-get`, a bounded results fragment, `hx-include` for the sibling fields),
  the `showModal()` dialog pattern, the shared page header.
- Tailwind CSS must be regenerated in every slice that adds a class token and
  committed: `text-bg` is not in the committed stylesheet today.
- **The two suite rules the previous feature learned the hard way apply here**:
  run the FULL browser suite for any slice that changes list or form markup, and
  choose the focused Rust filter to include the tests that assert markup a slice
  deletes — `/purchases/new` has a guard entry and at least three tests.
- RDD is **OFF**: ordinary checks only.
- Delivery: work-unit commits on a branch, no push or PR unless asked.

## Slices

- **T1 — the primary action colour.** `page_header.html` to mint with dark text,
  regenerate the stylesheet, verify the seven pages that share it. Independent of
  everything else.
- **T1b — the purchase row stops being blue.** The row anchor names `text-text`
  so it stops inheriting the anchor default, the way the products and suppliers
  rows already do. Independent, and small — but it is the visible half of the
  complaint.
- **T1c — retire the blue overrides.** The base `button` element is already
  mint with a dark label (`assets/tailwind.css`: `bg-accent … text-[#0a0f0d]`,
  plus its own hover and disabled states), so the ten buttons carrying
  `bg-accent2 text-white` are not a second palette — they are overrides fighting
  the base style. Removing those two classes lets every action in the app inherit
  one colour without inventing one. Decided by the user 2026-09-23.
- **T2 — the supplier picker.** `GET /web/supplier-search?q=` (bounded,
  read-only), a results fragment, the shared widget, and name-to-id resolution in
  the handlers that accept a supplier.
- **T3 — the creation flow.** `/purchases/new` deleted; the header component
  gains a dialog-opening action; the dialog holds the picker, pre-filled with the
  last-used supplier (a new read on the purchase repository); choosing creates the
  draft and lands on its record page.
- **T4 — the inline header.** The record page's header becomes an always-editable
  form for drafts (supplier picker, date, invoice, notes) posting to the existing
  header route; the Edit header dialog and its `⋯` entry are deleted.

Order: T1, T1b and T1c are independent of everything else; T2 before T3 and T4,
since both host the picker. T1, T1b, T1c and T2 are done; T3 remains, then T4.

## Acceptance criteria

- [ ] AC1: every page action renders in the mint accent with a dark label, and no
      page action uses `bg-accent2`.
- [ ] AC1b: the purchase list row's identifier, supplier, meta line and total all
      render in the normal text colour, not the anchor default — asserted on the
      row's own classes, not on one span inside it.
- [ ] AC2: `/purchases/new` no longer exists — no route, no template, no guard
      entry, no README mention, and nothing links to it.
- [ ] AC3: pressing `New purchase` opens a dialog whose supplier field is
      pre-filled with the last used supplier, and choosing a supplier creates the
      draft and lands on `/purchases/{id}`.
- [ ] AC4: the supplier field searches as you type and shows bounded matches;
      typing an exact name and submitting works without clicking a result.
- [ ] AC5: on a draft's record page the supplier, purchase date, supplier invoice
      no and notes are editable in place, and saving updates the document.
- [ ] AC6: the Edit header dialog and its `⋯` entry are gone, and no test asserts
      them.
- [ ] AC7: a principal without `purchases.create` is offered no creation action
      and cannot reach the creation path.
- [ ] AC8: `cargo test` green and the FULL browser suite green at every slice,
      with the stylesheet regenerated wherever a class token was added.

## Open decisions

- **The rest of the blue, after T1c.** Retiring the button overrides leaves
  `--color-accent2` doing three other jobs, and they are a different kind of
  thing from an action: it is the **base anchor colour** (`a { text-accent2 }`,
  so every link in the app), the **input focus ring** (`focus:border-accent2`),
  and the **row hover border** (`hover:border-accent2`, six of them: the user,
  customer, role, supplier and account lists, plus the supplier drawer's purchase
  rows). Read that way the palette is coherent — mint means "do this", blue
  means "you can interact here" — and the purchase list row is the odd one out
  because it has no hover border at all. The user's call: leave blue as the
  affordance colour, or finish the job and make it all mint.
- **The picker gates, resolved (2026-09-23).** The user's decision: loading lines
  keeps requiring `inventory.read`, and a role that can create purchases without
  it is the administrator's problem to fix from the role configuration. So the
  product search is **not** widened, and the creation flow may legitimately end
  for such a principal at the line picker. Recorded so the asymmetry is a
  decision rather than a surprise.

## Known pre-existing items, not fixed

- **Destructive dialog submits are not danger-tinted.** The purchase record
  page's `Discard draft` and `Cancel purchase` menu entries are `text-danger`,
  but their dialog submit buttons are `border border-border bg-transparent
  text-text` — neutral. They are also the two buttons that *lost* their blue
  override in an earlier iteration of this design and were already neutral
  before T1c, so T1c neither caused nor changed this; a destructive confirm
  simply does not look destructive. Found by the T1c verification, out of its
  scope, and worth deciding separately.
- **Two dark labels, imperceptibly different.** The page action is an anchor
  carrying `text-bg` (`#0f1115`); the base button's label is `#0a0f0d`. Both
  near-black, both asserted exactly by the new browser tests so neither drifts.
  Unifying them would be cosmetic.

## Progress

| Slice | Status | Commits / evidence |
|-------|--------|--------------------|
| T1 | **done** | `f1e27e7` — `page_header.html` to `bg-accent text-bg`, stylesheet regenerated. Verifier verdict *pass*: the whole-file stylesheet rule diff removes **zero** rules and adds exactly the two needed (so no other page lost a utility — checked mechanically over all 52 templates), the contrast goes from 2.54:1 (fails AA) to **12.40:1 (AAA)**, and the test asserts the rendered action tag in both directions. |
| T1b | **done** | `ac49610` — the row anchor names `text-text`. The verifier measured mint = `--color-accent` (not the money green), confirmed the peek contract survived byte-identical apart from the added class, and confirmed the test asserts the **row's opening tag** rather than an inner span, which is the narrow check that let this ship. |
| T1c | **done** | `c6537d9` — the ten overrides retired, stylesheet regenerated, plus the browser colour assertions. Verifier verdict *pass*: the rule-level stylesheet diff is exactly three removals and zero additions (checked character-level), nothing anywhere uses a white utility so the dropped `--color-white` token had no other user, all ten buttons lost only those two classes (ids, `onclick` and the conditional `disabled` intact), and the two tests are real gates. Its one substantive note is now closed: no test asserted a **rendered** colour, which is the class of defect that let the blue row ship, so two browser tests read `getComputedStyle` off the real elements. They were written against the token values before their first run and passed unchanged. |
| T2 | **done** | `03d7a5a` — the endpoint, two bounded service reads, the shared widget and the results fragment. Verifier verdict *pass*: the gate is proven a **restatement, not a widening** (at HEAD a `purchases.create`-only principal already received the whole roster, id/name/active flag, from the creation page), the matching shape mirrors the product picker exactly (same fold, same ten-row bound, same ordering), and the htmx claim — `hx-vals` only fills missing keys, so relying on `hx-include` alone would post the hidden CURRENT supplier — was checked against the vendored htmx 1.9.12 source. Its one real finding was a genuine defect, fixed and pinned before the commit: resolution ran through the bounded search, so with more than ten partial matches two colliding exact names could straddle the bound and one would be **silently chosen**, or an existing name reported as missing. Both failure modes were red first. |
| T3 | not started | — |
| T4 | not started | — |

Verified after both: `cargo test` 859 passed / 0 failed, full browser suite 85 passed / 0 failed / 4 skipped.

