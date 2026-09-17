-- Customer receipts (M4 clientes, slice L): link a sale payment to the receipt
-- that groups it. NULL means a direct payment on a single sale, which stays valid
-- exactly as before. ON DELETE RESTRICT: deleting a receipt that still groups a
-- payment is refused by the database, so the grouping document cannot vanish
-- while it still explains payments.
ALTER TABLE sale_payments
    ADD COLUMN receipt_id INTEGER NULL REFERENCES customer_receipts(id) ON DELETE RESTRICT;

CREATE INDEX IF NOT EXISTS idx_sale_payments_receipt_id
    ON sale_payments(receipt_id);

-- A payment may only be grouped under a receipt of its own customer: otherwise
-- the receipt's derived total would claim money its own collection never applied.
-- This is a data invariant (same technique as the walk-in triggers), so it fires
-- for direct SQL too. Only rows with a non-null receipt_id are checked, and
-- ungrouped payments or same-customer groupings are untouched.
CREATE TRIGGER IF NOT EXISTS trg_sale_payments_receipt_customer_insert
BEFORE INSERT ON sale_payments
FOR EACH ROW
WHEN NEW.receipt_id IS NOT NULL
 AND (SELECT customer_id FROM customer_receipts WHERE id = NEW.receipt_id)
  <> (SELECT customer_id FROM sales WHERE id = NEW.sale_id)
BEGIN
    SELECT RAISE(ABORT, 'sale payment cannot be grouped under another customer''s receipt');
END;

CREATE TRIGGER IF NOT EXISTS trg_sale_payments_receipt_customer_update
BEFORE UPDATE ON sale_payments
FOR EACH ROW
WHEN NEW.receipt_id IS NOT NULL
 AND (SELECT customer_id FROM customer_receipts WHERE id = NEW.receipt_id)
  <> (SELECT customer_id FROM sales WHERE id = NEW.sale_id)
BEGIN
    SELECT RAISE(ABORT, 'sale payment cannot be grouped under another customer''s receipt');
END;
