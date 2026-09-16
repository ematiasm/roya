-- Money traceability (M0 + M2/M3): give every finance transaction the source
-- document number as an opaque `reference`, and link each payment row to the
-- transaction it created. The foreign keys always point from the PAYMENT to the
-- TRANSACTION; finance never gains any knowledge of sales or purchases.
--
-- Backfill is deterministic only: `reference` is copied from `description` when
-- the description exactly matches the document number shape written by M2/M3
-- before this change (YYYY-SALE-NNNNNN / YYYY-PURCH-NNNNNN). Unrelated
-- descriptions stay NULL. Historical payment rows stay unlinked: when several
-- transactions share a document number there is no way to know which one a
-- payment created, but a row with `reference` set is still fully traceable to
-- its document.

ALTER TABLE transactions ADD COLUMN reference TEXT NULL;

UPDATE transactions
SET reference = description
WHERE description GLOB '[0-9][0-9][0-9][0-9]-SALE-[0-9][0-9][0-9][0-9][0-9][0-9]'
   OR description GLOB '[0-9][0-9][0-9][0-9]-PURCH-[0-9][0-9][0-9][0-9][0-9][0-9]';

-- payment -> transaction links. The transaction is RESTRICTed so a linked
-- movement can never be deleted out from under the payment that produced it.
ALTER TABLE sale_payments
    ADD COLUMN transaction_id INTEGER NULL REFERENCES transactions(id) ON DELETE RESTRICT;
ALTER TABLE sale_payments
    ADD COLUMN refund_transaction_id INTEGER NULL REFERENCES transactions(id) ON DELETE RESTRICT;

ALTER TABLE purchase_payments
    ADD COLUMN transaction_id INTEGER NULL REFERENCES transactions(id) ON DELETE RESTRICT;
ALTER TABLE purchase_payments
    ADD COLUMN refund_transaction_id INTEGER NULL REFERENCES transactions(id) ON DELETE RESTRICT;

CREATE INDEX IF NOT EXISTS idx_sale_payments_transaction_id
    ON sale_payments(transaction_id);
CREATE INDEX IF NOT EXISTS idx_sale_payments_refund_transaction_id
    ON sale_payments(refund_transaction_id);
CREATE INDEX IF NOT EXISTS idx_purchase_payments_transaction_id
    ON purchase_payments(transaction_id);
CREATE INDEX IF NOT EXISTS idx_purchase_payments_refund_transaction_id
    ON purchase_payments(refund_transaction_id);
