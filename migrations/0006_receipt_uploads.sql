-- Receipts can also be files kept in an S3 bucket. SQLite cannot widen a
-- CHECK, so the table is rebuilt (it holds only links, and is small).
CREATE TABLE receipt_new (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    donation_id  INTEGER NOT NULL REFERENCES donation (id) ON DELETE RESTRICT,
    store        TEXT    NOT NULL CHECK (store IN ('paperless', 's3')),
    -- paperless: the document id. s3: the object key.
    reference    TEXT    NOT NULL CHECK (length(reference) BETWEEN 1 AND 500),
    title        TEXT    NOT NULL DEFAULT '',
    -- Uploaded files only.
    content_type TEXT,
    size_bytes   INTEGER,
    sha256       TEXT,
    linked_by    TEXT    NOT NULL,
    linked_at    TEXT    NOT NULL,
    UNIQUE (donation_id, store, reference)
);
INSERT INTO receipt_new (id, donation_id, store, reference, title, linked_by, linked_at)
    SELECT id, donation_id, store, reference, title, linked_by, linked_at FROM receipt;
DROP TABLE receipt;
ALTER TABLE receipt_new RENAME TO receipt;
CREATE INDEX receipt_by_donation ON receipt (donation_id);
