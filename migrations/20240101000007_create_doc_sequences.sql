-- Doc sequences for M2 sales (first consumer: SALE).
-- PK(doc_type, year). Row ('SALE', YYYY) created lazily on first confirm.
CREATE TABLE IF NOT EXISTS doc_sequences (
    doc_type TEXT NOT NULL,
    year INTEGER NOT NULL,
    last_number INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (doc_type, year)
);
