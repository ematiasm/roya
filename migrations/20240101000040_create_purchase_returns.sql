-- Purchase returns: the business returns goods to a supplier.
--
-- A return is a DOCUMENT, not a movement. It is the answer to "I bought three
-- and I am sending two back", which the annulment path cannot express: cancel is
-- all-or-nothing and it discards the document rather than reversing part of it.
-- See odd/tasks/purchase-returns-and-credit-notes.md.
--
-- return_number UNIQUE NULL: NULL while Draft, assigned on confirm as
-- YYYY-PRET-NNNNNN via the doc_sequences consumer PRET. That table has no CHECK
-- on doc_type and creates its rows lazily, so the new consumer costs no
-- migration — only a literal and a format function.
-- SQLite treats NULLs as distinct in UNIQUE, so multiple Drafts with NULL are
-- allowed, exactly as with purchase_number.
--
-- supplier_id NOT NULL RESTRICT: history survives, deactivate the supplier. It
-- is copied from the parent purchase and exists on the return so the document
-- is self-contained on its own page.
-- purchase_id NOT NULL RESTRICT: the parent is confirmed history and is never
-- deleted under it. RESTRICT rather than CASCADE because a return is evidence
-- ABOUT a purchase, and deleting the evidence because the subject was deleted
-- is the wrong direction.
--
-- NO payment_type COLUMN. A return's refunds are determined entirely by the
-- parent's payment rows — which account each payment came from, how much was
-- collected, when — so a second copy of the flag here could disagree with the
-- rows it summarizes. The service reads the parent's. This is deliberate and it
-- is the one column where a return departs from purchases.
--
-- return_date is the day the return is MADE, not the parent's purchase_date: the
-- goods leave today and the money arrives today, and dating the document to the
-- purchase would put stock movements in the past.
--
-- qty and unit_cost are TEXT like finance and inventory. Bounds are enforced by
-- the service: SQLite TEXT cannot compare decimals safely in a CHECK. unit_cost
-- is FROZEN from the parent line when the return line is added, for the same
-- reason migration 39 froze the tax breakdown on a sale line — a fact a later
-- rule could move must not be able to rewrite a document. A return is always at
-- the purchase price, so this is a copy of a frozen value rather than a new
-- price, and the form therefore has no price field at all.
--
-- No `updated_at` trigger: the DEFAULT covers the insert, and SQLite only allows
-- a constant default on ADD COLUMN, which is the situation the legacy
-- trg_*_set_updated_at triggers exist to patch. Stamping updated_at on mutation
-- is the application's job, written into the UPDATE.
CREATE TABLE purchase_returns (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    return_number TEXT NULL UNIQUE,
    supplier_id INTEGER NOT NULL REFERENCES suppliers(id) ON DELETE RESTRICT,
    purchase_id INTEGER NOT NULL REFERENCES purchases(id) ON DELETE RESTRICT,
    status TEXT NOT NULL CHECK (status IN ('Draft', 'Confirmed', 'Cancelled')),
    return_date TEXT NOT NULL,
    notes TEXT NOT NULL DEFAULT '',
    cancel_reason TEXT NULL,
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    confirmed_at TEXT NULL,
    cancelled_at TEXT NULL,
    FOREIGN KEY (supplier_id) REFERENCES suppliers(id) ON DELETE RESTRICT,
    FOREIGN KEY (purchase_id) REFERENCES purchases(id) ON DELETE RESTRICT,
    FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE RESTRICT,
    FOREIGN KEY (updated_by) REFERENCES users(id) ON DELETE RESTRICT
);

CREATE INDEX idx_purchase_returns_status ON purchase_returns(status);
CREATE INDEX idx_purchase_returns_supplier_id ON purchase_returns(supplier_id);
CREATE INDEX idx_purchase_returns_purchase_id ON purchase_returns(purchase_id);
CREATE INDEX idx_purchase_returns_return_date ON purchase_returns(return_date);

-- Purchase return lines.
--
-- return_id CASCADE: lines die with the document.
-- purchase_line_id RESTRICT: the parent's line is confirmed history, and a
-- return is evidence about it.
--
-- UNIQUE (return_id, purchase_line_id): one line per parent line. A return's
-- quantity is a quantity OF that parent line, and splitting one parent line into
-- two return lines has no defined answer — the same argument that makes a
-- purchase refuse a repeated product, and it holds here for the same reason the
-- cost satellite holds one price per (product, supplier).
CREATE TABLE purchase_return_lines (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    return_id INTEGER NOT NULL REFERENCES purchase_returns(id) ON DELETE CASCADE,
    purchase_line_id INTEGER NOT NULL REFERENCES purchase_lines(id) ON DELETE RESTRICT,
    qty TEXT NOT NULL,
    unit_cost TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (return_id) REFERENCES purchase_returns(id) ON DELETE CASCADE,
    FOREIGN KEY (purchase_line_id) REFERENCES purchase_lines(id) ON DELETE RESTRICT,
    UNIQUE (return_id, purchase_line_id)
);

CREATE INDEX idx_purchase_return_lines_return_id ON purchase_return_lines(return_id);
CREATE INDEX idx_purchase_return_lines_purchase_line_id ON purchase_return_lines(purchase_line_id);

-- Purchase return payments: the money coming BACK from the supplier.
--
-- A refund is money entering, so it is an Income and the overdraft guard never
-- fires on it — the same reason an annulment refund is an Income. That is what
-- distinguishes this table from customer_return_payments.
--
-- Both link columns are present from the start, as purchase_payments and
-- sale_payments were in migration 19, rather than adding refund_transaction_id by
-- ALTER when the annulment path arrives. transaction_id and refund_transaction_id
-- are RESTRICT so a linked movement can never be deleted out from under the
-- payment that produced it.
--
-- The refund is capped at what the parent has actually COLLECTED, so a return on
-- a purchase that is confirmed but unpaid writes no payment row. The return
-- still exists to return the goods.
CREATE TABLE purchase_return_payments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    return_id INTEGER NOT NULL REFERENCES purchase_returns(id) ON DELETE CASCADE,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
    method_id INTEGER NOT NULL REFERENCES payment_methods(id) ON DELETE RESTRICT,
    amount TEXT NOT NULL,
    date TEXT NOT NULL,
    transaction_id INTEGER NULL REFERENCES transactions(id) ON DELETE RESTRICT,
    refund_transaction_id INTEGER NULL REFERENCES transactions(id) ON DELETE RESTRICT,
    created_by INTEGER NOT NULL REFERENCES users(id) ON DELETE RESTRICT,
    updated_by INTEGER NULL REFERENCES users(id) ON DELETE RESTRICT,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (return_id) REFERENCES purchase_returns(id) ON DELETE CASCADE,
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE RESTRICT,
    FOREIGN KEY (method_id) REFERENCES payment_methods(id) ON DELETE RESTRICT,
    FOREIGN KEY (transaction_id) REFERENCES transactions(id) ON DELETE RESTRICT,
    FOREIGN KEY (refund_transaction_id) REFERENCES transactions(id) ON DELETE RESTRICT,
    FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE RESTRICT,
    FOREIGN KEY (updated_by) REFERENCES users(id) ON DELETE RESTRICT
);

CREATE INDEX idx_purchase_return_payments_return_id ON purchase_return_payments(return_id);
CREATE INDEX idx_purchase_return_payments_account_id ON purchase_return_payments(account_id);
CREATE INDEX idx_purchase_return_payments_method_id ON purchase_return_payments(method_id);
CREATE INDEX idx_purchase_return_payments_transaction_id ON purchase_return_payments(transaction_id);
CREATE INDEX idx_purchase_return_payments_refund_transaction_id ON purchase_return_payments(refund_transaction_id);
