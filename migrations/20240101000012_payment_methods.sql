-- Payment methods (M0) + account allowlist + sale_payments.method_id.
-- Methods seeded without Other. Allowlist PK(account_id, method_id) RESTRICT both.
-- sale_payments.method_id RESTRICT NOT NULL (DEFAULT 1 only backfills pre-patch rows; app always writes explicit id).

CREATE TABLE IF NOT EXISTS payment_methods (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    is_active INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT OR IGNORE INTO payment_methods (name, is_active) VALUES
    ('Cash', 1),
    ('Transfer', 1),
    ('Debit', 1),
    ('CreditCard', 1),
    ('QR', 1);

CREATE TABLE IF NOT EXISTS account_payment_methods (
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
    method_id INTEGER NOT NULL REFERENCES payment_methods(id) ON DELETE RESTRICT,
    PRIMARY KEY (account_id, method_id),
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE RESTRICT,
    FOREIGN KEY (method_id) REFERENCES payment_methods(id) ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS idx_apm_account ON account_payment_methods(account_id);
CREATE INDEX IF NOT EXISTS idx_apm_method ON account_payment_methods(method_id);

-- sale_payments gains method_id. Pre-patch rows (if any) backfill to Cash (id 1).
ALTER TABLE sale_payments ADD COLUMN method_id INTEGER NOT NULL DEFAULT 1 REFERENCES payment_methods(id) ON DELETE RESTRICT;

CREATE INDEX IF NOT EXISTS idx_sale_payments_method_id ON sale_payments(method_id);

-- Sensible combos for well-known accounts (idempotent, only where accounts exist;
-- fresh DBs get methods now and combos via PaymentMethodService::ensure_defaults_for_account
-- when Caja/Banco/MP are created; no accounts are created here to keep installs explicit).
INSERT OR IGNORE INTO account_payment_methods (account_id, method_id)
SELECT a.id, m.id FROM accounts a JOIN payment_methods m ON
    ((a.name = 'Caja' AND m.name = 'Cash') OR
     (a.name = 'Banco' AND m.name IN ('Transfer', 'Debit', 'CreditCard')) OR
     (a.name = 'MP' AND m.name IN ('QR', 'Transfer')));
