# Design: move-cost-warning-to-confirm

## Why confirmed and not draft

A draft line's cost is provisional: the line can still be edited or deleted, and the purchase
may never be confirmed at all. A warning there asserted a comparison the domain did not yet
know — it presented a stale cost as a fact while the document that would make it one did not
exist. Confirming is the moment the cost becomes a fact: the line is frozen, the satellite is
updated, and the comparison means something. Warning **after** the document exists is also
operationally better — the confirm response itself brings the warning to the operator right
after the click, instead of asking them to act inside a document that might never ship.

## The rule inversion

The earlier change (`2026-09-21-add-cost-price-freshness`) justified its draft-only gate as
"the action only makes sense while the purchase is editable". The new rule is the opposite —
it makes sense precisely **after** the document exists — and the old justification no longer
holds: editability was never the property that made applying correct, existence of the
recorded cost was. The old wording is left in the archived folder untouched; this change
supersedes the timing, and the archive keeps the record of what was believed at the time.

## Why only `Confirmed`

Decided scope is `Confirmed` only. A purchase confirmed and later **cancelled** is a historical
document: it shows nothing. Its costs were real when confirmed, but the document is closed and
feeding its costs into a product update from a cancelled record would be a silent write from a
document that no longer moves goods or money. The gate is a single status comparison against
`Confirmed`, in the service's own derivation and again in the handler, so neither the template
nor a future caller can re-enable it.

## Layout consequence: colspan 5 → 4

The warning renders as a full-width sub-row under the line row. On a draft, the line table has
five header cells (the remove column is draft-only); on a confirmed purchase, the remove column
disappears and the table renders **four** header cells, so the warning row's `colspan` drops
from 5 to 4 (`templates/partials/purchase_detail.html:81`). The sub-row still sits underneath
the line row, never inside it, so the browser suite's assertion on the line row's text order
is untouched.

## Known divergences from the product drawer's badge — by design

The drawer's badge and this warning compare **different pairs**:

- the drawer badge compares the supplier **reference cost** (preferred supplier's current cost,
  else the cheapest) against the stored cost;
- this warning compares **this line's cost** against the stored cost.

They can therefore disagree: on a cost **decrease** (the warning is strictly-higher only, the
badge fires on any disagreement), with **several suppliers** (a non-preferred supplier's line
cost may differ from the reference cost the badge uses), or **after a confirmed-then-cancelled
purchase** (the line stops warning while the badge keeps comparing the satellite). This is by
design rather than a defect: the badge is a permanent standing signal, the warning is an
ephemeral, line-scoped prompt to act on the exact cost just recorded. The drawer badge covers
any disagreement the warning deliberately does not.
