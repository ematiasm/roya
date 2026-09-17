-- M4 customers (slice K2). `sales.customer_id` becomes mandatory: every sale has
-- an owner, and cash is anonymous only by booking it to the seeded walk-in.
-- SQLite cannot add a NOT NULL column with a REFERENCES clause in place, so this
-- is a controlled table rebuild (same technique as the stock_movements CHECK
-- expansions in migration 11).
--
-- `sales` is the parent of `sale_lines` and `sale_payments` (both ON DELETE
-- CASCADE), so dropping it while foreign keys are enabled fires that cascade and
-- would silently destroy every line and payment. The child rows are therefore
-- stashed in backup tables, the parent is swapped, and the child rows are
-- restored under the same ids before the backups are dropped. All existing
-- headers keep their `customer_name` snapshot and are backfilled to the seeded
-- walk-in.
CREATE TABLE sales_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    sale_number TEXT NULL UNIQUE,
    status TEXT NOT NULL CHECK (status IN ('Draft', 'Confirmed', 'Cancelled')),
    payment_type TEXT NOT NULL CHECK (payment_type IN ('Cash', 'Credit')),
    customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE RESTRICT,
    customer_name TEXT NOT NULL DEFAULT '',
    sale_date TEXT NOT NULL,
    due_date TEXT NULL,
    receipt_no TEXT NULL UNIQUE,
    notes TEXT NOT NULL DEFAULT '',
    cancel_reason TEXT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    confirmed_at TEXT NULL,
    cancelled_at TEXT NULL
);

INSERT INTO sales_new (id, sale_number, status, payment_type, customer_id,
                       customer_name, sale_date, due_date, receipt_no, notes,
                       cancel_reason, created_at, updated_at, confirmed_at, cancelled_at)
SELECT s.id, s.sale_number, s.status, s.payment_type,
       (SELECT c.id FROM customers c WHERE c.is_walkin = 1),
       s.customer_name, s.sale_date, s.due_date, s.receipt_no, s.notes,
       s.cancel_reason, s.created_at, s.updated_at, s.confirmed_at, s.cancelled_at
FROM sales s;

CREATE TABLE sale_lines_backup AS SELECT * FROM sale_lines;
CREATE TABLE sale_payments_backup AS SELECT * FROM sale_payments;

DROP TABLE sales;
ALTER TABLE sales_new RENAME TO sales;

INSERT INTO sale_lines (id, sale_id, product_id, qty, unit_price, created_at)
    SELECT id, sale_id, product_id, qty, unit_price, created_at
    FROM sale_lines_backup;
INSERT INTO sale_payments (id, sale_id, account_id, method_id, amount, date,
                           transaction_id, refund_transaction_id, created_at)
    SELECT id, sale_id, account_id, method_id, amount, date,
           transaction_id, refund_transaction_id, created_at
    FROM sale_payments_backup;

DROP TABLE sale_lines_backup;
DROP TABLE sale_payments_backup;

CREATE INDEX IF NOT EXISTS idx_sales_status ON sales(status);
CREATE INDEX IF NOT EXISTS idx_sales_sale_date ON sales(sale_date);
CREATE INDEX IF NOT EXISTS idx_sales_customer_id ON sales(customer_id);
