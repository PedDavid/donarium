-- Facts about the database itself.
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Donations. Append-only: a mistake is corrected with a void entry, never by
-- editing or deleting, so the history stays trustworthy (see the triggers).
CREATE TABLE donation (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    -- A project with donations cannot be deleted.
    project_id        INTEGER NOT NULL REFERENCES project (id) ON DELETE RESTRICT,
    platform          TEXT    NOT NULL,
    -- What was paid, as on the receipt: whole minor units (cents) of `currency`.
    amount_minor      INTEGER NOT NULL CHECK (amount_minor > 0),
    currency          TEXT    NOT NULL CHECK (length(currency) = 3 AND currency = upper(currency)),
    donated_on        TEXT    NOT NULL
                      CHECK (donated_on GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'),
    -- The same amount in the database's base currency, converted once when it
    -- was recorded. All three are set together, or none while it is pending.
    base_amount_minor INTEGER CHECK (base_amount_minor IS NULL OR base_amount_minor > 0),
    fx_rate           REAL    CHECK (fx_rate IS NULL OR fx_rate > 0),
    fx_source         TEXT    CHECK (fx_source IS NULL OR fx_source IN ('same', 'ecb', 'manual')),
    kind              TEXT    NOT NULL DEFAULT 'one_off' CHECK (kind IN ('one_off', 'recurring')),
    note              TEXT    NOT NULL DEFAULT '',
    recorded_by       TEXT    NOT NULL,
    recorded_by_name  TEXT    NOT NULL DEFAULT '',
    recorded_at       TEXT    NOT NULL,
    -- Set on a void entry: the donation it cancels.
    voids_id          INTEGER REFERENCES donation (id),
    CHECK ((base_amount_minor IS NULL) = (fx_rate IS NULL)
       AND (fx_rate IS NULL) = (fx_source IS NULL))
);

-- A donation is voided at most once.
CREATE UNIQUE INDEX donation_voided_once ON donation (voids_id) WHERE voids_id IS NOT NULL;
CREATE INDEX donation_by_project ON donation (project_id, donated_on);
CREATE INDEX donation_pending_conversion ON donation (id) WHERE base_amount_minor IS NULL;

CREATE TRIGGER donation_never_deleted BEFORE DELETE ON donation
BEGIN
    SELECT RAISE(ABORT, 'donations are never deleted');
END;

-- The one allowed change: a pending base amount can be filled in, once.
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
BEGIN
    SELECT RAISE(ABORT, 'donations are append-only');
END;

-- Exchange rates already fetched: one unit of `currency` in `base`, on `date`.
CREATE TABLE fx_rate (
    date     TEXT NOT NULL,
    currency TEXT NOT NULL,
    base     TEXT NOT NULL,
    rate     REAL NOT NULL CHECK (rate > 0),
    -- The date the rate is actually for: the previous business day when `date`
    -- was a weekend or a holiday.
    rate_date TEXT NOT NULL,
    PRIMARY KEY (date, currency, base)
);
