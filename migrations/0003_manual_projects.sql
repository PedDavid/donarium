-- Settings an admin makes for a project, discovered or added by hand.
CREATE TABLE project_override (
    project_id      INTEGER PRIMARY KEY REFERENCES project (id) ON DELETE CASCADE,
    -- Usage you assign yourself, added to what scans found. It is how a
    -- project that no scan can see (an app you run, say) gets a score.
    manual_usage    REAL    NOT NULL DEFAULT 0 CHECK (manual_usage >= 0 AND manual_usage <= 1000),
    -- E.g. a company-backed project you do not intend to fund.
    ignore          INTEGER NOT NULL DEFAULT 0 CHECK (ignore IN (0, 1)),
    notes           TEXT    NOT NULL DEFAULT '',
    updated_by      TEXT    NOT NULL,
    updated_by_name TEXT    NOT NULL DEFAULT '',
    updated_at      TEXT    NOT NULL
);
