-- Recurring donations (a monthly or yearly subscription on a platform). A plan
-- produces an ordinary, append-only donation each time one is due.
CREATE TABLE recurring (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    -- A project with a plan cannot be deleted.
    project_id       INTEGER NOT NULL REFERENCES project (id) ON DELETE RESTRICT,
    platform         TEXT    NOT NULL,
    amount_minor     INTEGER NOT NULL CHECK (amount_minor > 0),
    currency         TEXT    NOT NULL CHECK (length(currency) = 3 AND currency = upper(currency)),
    every            TEXT    NOT NULL CHECK (every IN ('monthly', 'yearly')),
    starts_on        TEXT    NOT NULL
                     CHECK (starts_on GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'),
    -- The last day a donation can fall on. Empty while it keeps running.
    ends_on          TEXT
                     CHECK (ends_on IS NULL OR ends_on GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'),
    note             TEXT    NOT NULL DEFAULT '',
    created_by       TEXT    NOT NULL,
    created_by_name  TEXT    NOT NULL DEFAULT '',
    created_at       TEXT    NOT NULL
);

-- Which plan and which due date a donation came from; each due date once.
ALTER TABLE donation ADD COLUMN recurring_id  INTEGER REFERENCES recurring (id) ON DELETE RESTRICT;
ALTER TABLE donation ADD COLUMN recurring_due TEXT;
CREATE UNIQUE INDEX donation_recurring_once ON donation (recurring_id, recurring_due)
    WHERE recurring_id IS NOT NULL;

DROP TRIGGER donation_append_only;
CREATE TRIGGER donation_append_only BEFORE UPDATE ON donation
WHEN OLD.base_amount_minor IS NOT NULL
  OR NEW.id IS NOT OLD.id
  OR NEW.project_id IS NOT OLD.project_id
  OR NEW.platform IS NOT OLD.platform
  OR NEW.amount_minor IS NOT OLD.amount_minor
  OR NEW.currency IS NOT OLD.currency
  OR NEW.donated_on IS NOT OLD.donated_on
  OR NEW.kind IS NOT OLD.kind
  OR NEW.note IS NOT OLD.note
  OR NEW.recorded_by IS NOT OLD.recorded_by
  OR NEW.recorded_by_name IS NOT OLD.recorded_by_name
  OR NEW.recorded_at IS NOT OLD.recorded_at
  OR NEW.voids_id IS NOT OLD.voids_id
  OR NEW.recurring_id IS NOT OLD.recurring_id
  OR NEW.recurring_due IS NOT OLD.recurring_due
BEGIN
    SELECT RAISE(ABORT, 'donations are append-only');
END;
