# Tasks: move-cost-warning-to-confirm

> Delivered on `fix/cost-warning-at-confirm` (gate inversion `f873305`); this folder is the
> change record, authored directly in its archive home. It supersedes the timing rule in
> `2026-09-21-add-cost-price-freshness`, whose folder is deliberately left untouched.

## Slices

- [x] T1: the gate inversion — `stale_cost` derived only on a `Confirmed` purchase in
      `PurchasesService::record_from_detail` (strictly-higher and non-zero-stored gates kept,
      draft and cancelled show nothing), the `web_apply_line_cost` handler refusing any
      non-confirmed status itself, and the route tests updated for the new gate (including the
      refusal of a draft and the confirmed-then-cancelled case).
      (2026-09-21: delivered as `f873305` on `fix/cost-warning-at-confirm`.)
- [x] T2: the browser journey updated — a confirmed purchase's line warns with both numbers,
      applying updates the product, and a draft shows no warning.
      (2026-09-21: delivered within `f873305`.)

## Verify

- [ ] Final verification is its own slice, not part of T1/T2: `cargo test` over the whole
      suite (expected 798 passed), and confirmation that
      `ac10_purchase_never_writes_cost_price_and_satellite_wins` passes unchanged.
