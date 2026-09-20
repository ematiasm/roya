-- no-transaction
-- Audit on the finance tables (M5 Phase B, slice S9): every row of `accounts`,
-- `transactions` and `payment_methods` records who created it (`created_by`,
-- NOT NULL) and, when applicable, who last changed it (`updated_by`, NULL). The
-- columns are ordinary integers with a RESTRICT foreign key to `users`; the
-- owning module writes them from the `Principal` each request resolves, so a
-- department never reads identity tables (spec invariant 1, AC20).
--
-- The columns cannot be `ALTER TABLE`d into existence: SQLite refuses a NOT NULL
-- column without a constant default, and a `REFERENCES` clause needs the table
-- rebuild anyway. Same technique as migration 21 (`add_sales_customer`) and
-- migration 24 (`payment_methods_single_account`), from which this file also
-- takes its first line: sqlx runs migrations inside a transaction where
-- PRAGMA foreign_keys is a no-op and a deferred violation from DROP TABLE
-- cannot be healed before COMMIT, so this migration is marked
-- `-- no-transaction` and disables foreign keys itself for the swap,
-- re-enabling after. Dropping a rebuilt parent (accounts is the parent of
-- transactions, payment_methods, sale_payments, purchase_payments and
-- customer_receipts; transactions and payment_methods are the parents of the
-- payment rows) would otherwise fire RESTRICT refusals or CASCADE deletions;
-- with keys off the drop is inert, child rows are untouched, and every id is
-- preserved by the INSERT ... SELECT, so the re-enabled constraints find the
-- same graph that existed before.
--
-- The actor for pre-existing rows: every row that exists when this migration
-- runs predates the audit, and no user may exist yet (migrations run before the
-- application's bootstrap creates the administrator), so `created_by NOT NULL
-- REFERENCES users(id)` would have nothing to point at. Rather than guess a
-- person, the migration creates its own sentinel: an inactive, roleless account
-- named `sistema` with a deliberately malformed password hash (`verify` treats
-- a malformed stored hash as a failed verification, a behaviour pinned by a
-- test in security/password.rs), and attributes every pre-existing row to it.
-- The bootstrap path is untouched: on a fresh install (or an upgrade whose
-- database has no users) the application's `bootstrap_admin` still creates the
-- administrator through its ordinary creation path, so exactly one active
-- administrator exists afterwards, holding the protected role. The sentinel is
-- inserted only when there is something to attribute; a database with none of
-- the three tables populated gains no user at all. The seeded payment methods
-- (migration 12) are such rows, so on a fresh install the sentinel exists — the
-- spec records this as the honest-attribution assumption of Phase B: the rows
-- that predate the audit were not created by any person the system knew, and
-- saying "Sistema" says exactly that, where a synthetic "administrator"
-- attribution would name a person who did not create them (the rejected
-- alternative: backfilling to the bootstrap administrator would both invent a
-- personal attribution and make this migration depend on the bootstrap having
-- run). The rejected alternative is documented in
-- openspec/changes/2026-09-19-add-actor-audit/spec.md.
--
-- Idempotent guards (same discipline as migration 27): the sentinel insert is
-- guarded with WHERE NOT EXISTS, so replaying or re-running the statement
-- cannot duplicate the row. If an operator already owns a user named
-- `sistema`, their row is the attribution target — the migration never
-- creates a second one.

PRAGMA foreign_keys = OFF;

INSERT INTO users (username, display_name, password_hash, is_active)
SELECT 'sistema',
       'Sistema (anterior al registro)',
       -- Malformed on purpose: `PasswordVerifier` answers false for a hash it
       -- cannot parse (tested in security/password.rs), and the account is
       -- inactive on top, so no credential can ever log it in.
       '$sentinel$no-login-credential$',
       0
WHERE NOT EXISTS (SELECT 1 FROM users WHERE username = 'sistema' COLLATE NOCASE)
  AND ( EXISTS (SELECT 1 FROM accounts)
     OR EXISTS (SELECT 1 FROM transactions)
     OR EXISTS (SELECT 1 FROM payment_methods) );

-- ---------------------------------------------------------------------------
-- accounts: rebuild with the audit columns.
-- ---------------------------------------------------------------------------
CREATE TABLE accounts_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    cached_balance TEXT NOT NULL DEFAULT '0',
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO accounts_new (id, name, cached_balance, created_by, updated_by, created_at)
SELECT id, name, cached_balance,
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE),
       NULL,
       created_at
FROM accounts;

DROP TABLE accounts;
ALTER TABLE accounts_new RENAME TO accounts;

CREATE INDEX IF NOT EXISTS idx_accounts_name ON accounts(name);

-- ---------------------------------------------------------------------------
-- transactions: rebuild with the audit columns. The account_id foreign key is
-- declared once (migration 2 carried it inline and at table level; the
-- semantics were always the same).
-- ---------------------------------------------------------------------------
CREATE TABLE transactions_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('Income', 'Expense')),
    amount TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    reference TEXT NULL,
    date TEXT NOT NULL,
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO transactions_new (id, account_id, kind, amount, description, reference, date, created_by, updated_by, created_at)
SELECT id, account_id, kind, amount, description, reference, date,
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE),
       NULL,
       created_at
FROM transactions;

DROP TABLE transactions;
ALTER TABLE transactions_new RENAME TO transactions;

CREATE INDEX IF NOT EXISTS idx_transactions_account_id ON transactions(account_id);
CREATE INDEX IF NOT EXISTS idx_transactions_date ON transactions(date);
CREATE INDEX IF NOT EXISTS idx_transactions_account_date ON transactions(account_id, date);

-- ---------------------------------------------------------------------------
-- payment_methods: rebuild with the audit columns. Ownership
-- (account_id NULL = unassigned), the UNIQUE(account_id, name) rule and every
-- id are preserved (migration 24's shape), so sale_payments,
-- purchase_payments and customer_receipts history keeps its method ids.
-- ---------------------------------------------------------------------------
CREATE TABLE payment_methods_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    account_id INTEGER NULL REFERENCES accounts(id) ON DELETE RESTRICT,
    is_active INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1)),
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (account_id, name)
);

INSERT INTO payment_methods_new (id, name, account_id, is_active, created_by, updated_by, created_at)
SELECT id, name, account_id, is_active,
       (SELECT id FROM users WHERE username = 'sistema' COLLATE NOCASE),
       NULL,
       created_at
FROM payment_methods;

DROP TABLE payment_methods;
ALTER TABLE payment_methods_new RENAME TO payment_methods;

CREATE INDEX IF NOT EXISTS idx_payment_methods_account ON payment_methods(account_id);

PRAGMA foreign_keys = ON;
