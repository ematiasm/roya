-- Migration 47: the missing edge from a delivery of money to the receipt that groups
-- it.
--
-- Why it was missing. P3 made a collection write ONE `payments` document covering N
-- invoices, and kept `sale_payments` as the pointer the receipt's own read uses. That
-- left the receipt with a single way to know how much money it grouped: summing its
-- allocations. Which is correct right up until a collection delivers MORE than the
-- outstanding debt — because the excess is not applied to anything, so it appears in no
-- allocation, and the receipt reports the 30 it applied while the box received 50.
--
-- The relationship is "a collection may group a delivery", and the side that can be
-- NULL is the DELIVERY: a direct payment on one sale has no receipt. Hence a nullable
-- column on `payments` rather than a `payment_id` on `customer_receipts`, which would
-- have forced one delivery per receipt and made the grouping document pointless.
--
-- ON DELETE RESTRICT, verified against SQLite rather than assumed: with the column in
-- place, deleting a receipt that groups a delivery FAILS. A grouping document cannot be
-- deleted out from under the money it grouped.
--
-- `payment_allocations` is deliberately NOT touched. The link is to the DELIVERY, not
-- to the shares: putting a receipt on each allocation would make the receipt sum shares
-- again, and the whole point is that the money and its attribution are two different
-- facts.

PRAGMA foreign_keys = ON;

ALTER TABLE payments ADD COLUMN receipt_id INTEGER NULL
    REFERENCES customer_receipts(id) ON DELETE RESTRICT;

CREATE INDEX idx_payments_receipt ON payments(receipt_id);
