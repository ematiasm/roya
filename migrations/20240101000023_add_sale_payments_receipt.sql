-- Customer receipts (M4 clientes, slice L): link a sale payment to the receipt
-- that groups it. NULL means a direct payment on a single sale, which stays valid
-- exactly as before. ON DELETE RESTRICT: deleting a receipt that still groups a
-- payment is refused by the database, so the grouping document cannot vanish
-- while the `receipt.total = SUM(allocations)` invariant still depends on it.
ALTER TABLE sale_payments
    ADD COLUMN receipt_id INTEGER NULL REFERENCES customer_receipts(id) ON DELETE RESTRICT;

CREATE INDEX IF NOT EXISTS idx_sale_payments_receipt_id
    ON sale_payments(receipt_id);
