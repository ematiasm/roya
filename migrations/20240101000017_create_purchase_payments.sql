-- Purchase payments (M3). Each row generates one M0 Expense with reference =
-- purchase_number (the money leaving the account for the payable). Decimal
-- amount (> 0) as TEXT like finance. purchase_id CASCADE; account_id and
-- method_id RESTRICT so history survives. The (account_id, method_id) pair must
-- exist in account_payment_methods (M0 allowlist), enforced by PurchasesService.
CREATE TABLE IF NOT EXISTS purchase_payments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    purchase_id INTEGER NOT NULL REFERENCES purchases(id) ON DELETE CASCADE,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
    method_id INTEGER NOT NULL REFERENCES payment_methods(id) ON DELETE RESTRICT,
    amount TEXT NOT NULL,
    date TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (purchase_id) REFERENCES purchases(id) ON DELETE CASCADE,
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE RESTRICT,
    FOREIGN KEY (method_id) REFERENCES payment_methods(id) ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS idx_purchase_payments_purchase_id ON purchase_payments(purchase_id);
CREATE INDEX IF NOT EXISTS idx_purchase_payments_account_id ON purchase_payments(account_id);
CREATE INDEX IF NOT EXISTS idx_purchase_payments_method_id ON purchase_payments(method_id);
