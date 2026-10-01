-- Customer returns: the customer returns goods to the business.
--
-- The mirror of purchase_returns, and every difference is a sign. Stock comes IN
-- with reason Sale-return, money goes OUT as an Expense, and the Expense is what
-- the overdraft guard fires on — the same asymmetry an annulment already has,
-- where a refund you owe cannot be blocked but money entering never is.
--
-- The Spanish label is "Nota de credito" and the English is "Customer return".
-- The identifier is deliberately NOT SaleReturn: that is the stock movement
-- reason, which describes a physical event and keeps its name. A document and a
-- movement must not share a word in code either, which is the ambiguity the
-- naming decision was made to remove.
--
-- The English stays plain on purpose. This app already renders "Anular venta" as
-- "Cancel sale" and "reembolsados" as "refunded" rather than "Annul" or
-- "reimbursed", so it avoids accounting jargon, and "credit note" is jargon. The
-- Spanish term is the more specific one; both catalogs describe the same
-- document.
--
-- The stock reason Sale-return is already valid in the live CHECK — migration 11
-- added it and migration 31 rebuilt the table with it — so this migration adds
-- no reason and changes no CHECK.
--
-- credit_note_number is named for the document rather than for the family, because
-- the document IS the credit note; the return_number on purchase_returns names
-- the family because there the document and the movement genuinely share a word.
--
-- NO payment_type COLUMN, for the same reason as purchase_returns: the parent's
-- payment rows say what was collected and to which account, and a second copy of
-- the flag here could disagree with them.
--
-- return_date is the day the return is MADE, so the goods come in today and the
-- refund leaves today.
--
-- unit_price is FROZEN from the parent sale line when the line is added, for the
-- reason migration 39 froze the tax breakdown: a fact a later rule could move must
-- not be able to rewrite a document. What is deliberately NOT frozen is the cost
-- the goods carried on the day they were sold — the app computes no margin today,
-- and sale_lines does not carry a cost either. That is a real gap for a future
-- margin report and it is recorded as open in the feature document rather than
-- paid for speculatively here.
CREATE TABLE customer_returns (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    credit_note_number TEXT NULL UNIQUE,
    customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE RESTRICT,
    sale_id INTEGER NOT NULL REFERENCES sales(id) ON DELETE RESTRICT,
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
    FOREIGN KEY (customer_id) REFERENCES customers(id) ON DELETE RESTRICT,
    FOREIGN KEY (sale_id) REFERENCES sales(id) ON DELETE RESTRICT,
    FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE RESTRICT,
    FOREIGN KEY (updated_by) REFERENCES users(id) ON DELETE RESTRICT
);

CREATE INDEX idx_customer_returns_status ON customer_returns(status);
CREATE INDEX idx_customer_returns_customer_id ON customer_returns(customer_id);
CREATE INDEX idx_customer_returns_sale_id ON customer_returns(sale_id);
CREATE INDEX idx_customer_returns_return_date ON customer_returns(return_date);

-- Credit note lines.
--
-- UNIQUE (return_id, sale_line_id) for the same reason as its purchase twin: a
-- return's quantity is a quantity OF that sale line, and splitting one line into
-- two credit note lines has no defined answer. Sales permit the same product on
-- two lines of one SALE, so two lines here can carry the same product — and the
-- pair is still the right key, because the pair is about lines, not products.
CREATE TABLE customer_return_lines (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    return_id INTEGER NOT NULL REFERENCES customer_returns(id) ON DELETE CASCADE,
    sale_line_id INTEGER NOT NULL REFERENCES sale_lines(id) ON DELETE RESTRICT,
    qty TEXT NOT NULL,
    unit_price TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    FOREIGN KEY (return_id) REFERENCES customer_returns(id) ON DELETE CASCADE,
    FOREIGN KEY (sale_line_id) REFERENCES sale_lines(id) ON DELETE RESTRICT,
    UNIQUE (return_id, sale_line_id)
);

CREATE INDEX idx_customer_return_lines_return_id ON customer_return_lines(return_id);
CREATE INDEX idx_customer_return_lines_sale_line_id ON customer_return_lines(sale_line_id);

-- Credit note payments: the money going back to the customer.
--
-- A refund is money LEAVING, so it is an Expense, and the overdraft guard DOES fire
-- on it — a refund the business cannot pay is refused rather than promised. That
-- is the whole difference from purchase_return_payments and it is deliberate.
--
-- Both link columns present from the start, as sale_payments and
-- purchase_payments were in migration 19.
CREATE TABLE customer_return_payments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    return_id INTEGER NOT NULL REFERENCES customer_returns(id) ON DELETE CASCADE,
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
    FOREIGN KEY (return_id) REFERENCES customer_returns(id) ON DELETE CASCADE,
    FOREIGN KEY (account_id) REFERENCES accounts(id) ON DELETE RESTRICT,
    FOREIGN KEY (method_id) REFERENCES payment_methods(id) ON DELETE RESTRICT,
    FOREIGN KEY (transaction_id) REFERENCES transactions(id) ON DELETE RESTRICT,
    FOREIGN KEY (refund_transaction_id) REFERENCES transactions(id) ON DELETE RESTRICT,
    FOREIGN KEY (created_by) REFERENCES users(id) ON DELETE RESTRICT,
    FOREIGN KEY (updated_by) REFERENCES users(id) ON DELETE RESTRICT
);

CREATE INDEX idx_customer_return_payments_return_id ON customer_return_payments(return_id);
CREATE INDEX idx_customer_return_payments_account_id ON customer_return_payments(account_id);
CREATE INDEX idx_customer_return_payments_method_id ON customer_return_payments(method_id);
CREATE INDEX idx_customer_return_payments_transaction_id ON customer_return_payments(transaction_id);
CREATE INDEX idx_customer_return_payments_refund_transaction_id ON customer_return_payments(refund_transaction_id);
