-- Receipts: a pointer from a donation to a document that lives elsewhere
-- (Paperless-ngx). Nothing is copied, only the reference and the title it had
-- when it was linked. Unlike donations, links can be removed.
CREATE TABLE receipt (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    donation_id INTEGER NOT NULL REFERENCES donation (id) ON DELETE RESTRICT,
    store       TEXT    NOT NULL CHECK (store IN ('paperless')),
    -- The document's id in that store.
    reference   TEXT    NOT NULL CHECK (length(reference) BETWEEN 1 AND 200),
    title       TEXT    NOT NULL DEFAULT '',
    linked_by   TEXT    NOT NULL,
    linked_at   TEXT    NOT NULL,
    UNIQUE (donation_id, store, reference)
);
CREATE INDEX receipt_by_donation ON receipt (donation_id);
