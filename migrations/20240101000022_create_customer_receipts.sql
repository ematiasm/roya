-- Customer receipts (M4 clientes, slice L). One receipt is one handover of money
-- that can cover several credit sales: it groups the per-sale payments the
-- collection produced, and each of those payments still owns its sale and posts
-- its own finance movement (`sale_payments.transaction_id`). The receipt posts no
-- movement of its own. There is deliberately NO total column: the amount handed
-- over is derived as the SUM of the payments grouped under the receipt, so an
-- interrupted collection can leave fewer payments but never a receipt claiming
-- more than it applied. The columns mirror `sale_payments` so both sides stay
-- comparable.
-- customer_id/account_id/method_id are RESTRICT: a receipt is history, so neither
-- the customer nor the finance pair it was recorded with can disappear under it.
-- The service validates the customer (404) and the (account, method) allowlist
-- (400) before any row is written.
CREATE TABLE IF NOT EXISTS customer_receipts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE RESTRICT,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
    method_id INTEGER NOT NULL REFERENCES payment_methods(id) ON DELETE RESTRICT,
    date TEXT NOT NULL,
    notes TEXT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_customer_receipts_customer_id
    ON customer_receipts(customer_id);
CREATE INDEX IF NOT EXISTS idx_customer_receipts_date
    ON customer_receipts(date);
