-- no-transaction
-- Payment methods become account-owned (1:N): each method belongs to at most one
-- account via `payment_methods.account_id` (NULL = unassigned, not usable for
-- payments) with UNIQUE(account_id, name), replacing the M:N allowlist
-- `account_payment_methods`, which is dropped at the end. Method ids are kept
-- stable, so `sale_payments`, `purchase_payments` and `customer_receipts` history
-- is untouched.
--
-- Backfill from the allowlist: a method allowed on exactly one account keeps its
-- row and id with that account; a method allowed on N>1 accounts keeps the
-- original row (and id) on the lowest account id and gains one duplicate row per
-- other account (same name, new id); a method with no allowlist row stays NULL.
--
-- The parent table is rebuilt (same technique as migration 21). sqlx runs
-- migrations inside a transaction, where PRAGMA foreign_keys is a no-op and a
-- deferred violation from DROP TABLE cannot be healed before COMMIT — so this
-- migration is marked `-- no-transaction` (first line) and disables foreign
-- keys itself for the swap, re-enabling after: with keys on, dropping the
-- parent while RESTRICT payment history references it would fail.

PRAGMA foreign_keys = OFF;

CREATE TABLE payment_methods_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    account_id INTEGER NULL REFERENCES accounts(id) ON DELETE RESTRICT,
    is_active INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (account_id, name)
);

-- Original rows keep their ids; account_id is the lowest allowlisted account, or
-- NULL when the method was never allowed anywhere (orphan).
INSERT INTO payment_methods_new (id, name, account_id, is_active, created_at)
SELECT m.id, m.name,
       (SELECT MIN(apm.account_id)
          FROM account_payment_methods apm
         WHERE apm.method_id = m.id),
       m.is_active, m.created_at
  FROM payment_methods m;

-- Duplicates for the other accounts of shared methods (same name, new ids).
INSERT INTO payment_methods_new (name, account_id, is_active, created_at)
SELECT m.name, apm.account_id, m.is_active, m.created_at
  FROM payment_methods m
  JOIN account_payment_methods apm ON apm.method_id = m.id
 WHERE apm.account_id <> (SELECT MIN(x.account_id)
                            FROM account_payment_methods x
                           WHERE x.method_id = m.id);

DROP TABLE account_payment_methods;
DROP TABLE payment_methods;
ALTER TABLE payment_methods_new RENAME TO payment_methods;

CREATE INDEX IF NOT EXISTS idx_payment_methods_account ON payment_methods(account_id);

PRAGMA foreign_keys = ON;
