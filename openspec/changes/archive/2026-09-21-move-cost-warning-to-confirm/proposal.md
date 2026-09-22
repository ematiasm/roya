# Proposal: move-cost-warning-to-confirm (M3 purchases)

## Problem statement

`2026-09-21-add-cost-price-freshness` delivered the stale-cost warning — and its
"Apply to product" action — on a **draft** purchase line. That timing asserted a comparison the
domain does not yet know: a draft line's cost is provisional (the line can still be edited or
deleted, and the purchase may never be confirmed at all), so warning there presented a stale
cost as fact. The old change even justified the draft gate as "the action only makes sense
while the purchase is editable" — the opposite of the truth: applying makes sense precisely
**after** the document exists, because that is when the recorded cost stops being provisional
and becomes a fact.

## What changes

- The stale-cost warning and the apply-cost action move from a draft gate to a **confirmed-only**
  gate: the flag is derived in `PurchasesService::record_from_detail` only when
  `status == Confirmed`, and the `web_apply_line_cost` handler refuses anything else.
- The two derivation gates from the earlier change are kept: the line cost must be **strictly
  higher** than the stored cost, and the stored cost must be **non-zero** (zero means "no cost
  recorded yet").
- A draft shows nothing, and a purchase confirmed and later cancelled shows nothing: the
  cancelled document is historical, and its costs must not feed a product update.
- AC10 is untouched: the action still writes through `InventoryService::update_product`, never
  through the purchase flow, and confirming still touches only the satellite.

**This change supersedes the timing rule in `openspec/changes/archive/2026-09-21-add-cost-price-freshness/`.**
That folder is left untouched — it is the record of what that change delivered at the time;
this folder names it as superseded instead.

## Scope

Documentation-only promotion plus the already-committed gate inversion (`f873305`, on
`fix/cost-warning-at-confirm`). Decided scope is `Confirmed` only — no third state, no
draft-phase preview.
