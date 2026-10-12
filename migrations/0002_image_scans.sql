-- Image scans. Rebuilds `source` (SQLite cannot change a CHECK in place) and
-- `usage` (its key now includes where the usage came from).
--
-- sqlx runs every SQLite migration in a transaction, where PRAGMA
-- foreign_keys = OFF has no effect. Dropping `source` while `usage` refers to it
-- would therefore delete every usage row through ON DELETE CASCADE. So the
-- child table is emptied into a backup and dropped first: nothing refers to
-- `source` by the time it is dropped, and `usage` is rebuilt afterwards.

CREATE TABLE usage_backup AS SELECT source_id, project_id, kind FROM usage;
DROP TABLE usage;

CREATE TABLE source_new (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    -- 'gitea' (also Forgejo) is accepted here already so that adding it later
    -- needs no table rebuild.
    provider         TEXT    NOT NULL CHECK (provider IN ('github', 'gitlab', 'gitea')),
    repo             TEXT    NOT NULL,
    weight           REAL    NOT NULL CHECK (weight > 0 AND weight <= 100),
    scan_method      TEXT    NOT NULL DEFAULT 'auto'
                     CHECK (scan_method IN ('auto', 'images', 'dependency-graph', 'sbom', 'lockfiles')),
    enabled          INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    last_scan_at     TEXT,
    last_scan_ok     INTEGER CHECK (last_scan_ok IN (0, 1)),
    last_scan_status TEXT,
    created_by       TEXT    NOT NULL,
    created_by_name  TEXT    NOT NULL DEFAULT '',
    created_at       TEXT    NOT NULL,
    updated_by       TEXT    NOT NULL,
    updated_by_name  TEXT    NOT NULL DEFAULT '',
    updated_at       TEXT    NOT NULL
);

INSERT INTO source_new (id, provider, repo, weight, scan_method, enabled,
                        last_scan_at, last_scan_status,
                        created_by, created_by_name, created_at,
                        updated_by, updated_by_name, updated_at)
SELECT id, provider, repo, weight, scan_method, enabled,
       last_scan_at, last_scan_status,
       created_by, created_by_name, created_at,
       updated_by, updated_by_name, updated_at
  FROM source;

DROP TABLE source;
ALTER TABLE source_new RENAME TO source;
CREATE UNIQUE INDEX source_repo ON source (provider, repo COLLATE NOCASE);

CREATE TABLE usage (
    source_id  INTEGER NOT NULL REFERENCES source (id) ON DELETE CASCADE,
    project_id INTEGER NOT NULL REFERENCES project (id) ON DELETE CASCADE,
    -- Which scan found it. Each scan rewrites only its own rows.
    via        TEXT    NOT NULL CHECK (via IN ('images', 'dependencies')),
    kind       TEXT    NOT NULL CHECK (kind IN ('direct', 'transitive')),
    PRIMARY KEY (source_id, project_id, via)
);

INSERT INTO usage (source_id, project_id, via, kind)
SELECT source_id, project_id, 'dependencies', kind FROM usage_backup;
DROP TABLE usage_backup;

-- Projects added by hand are never pruned when no source uses them.
ALTER TABLE project ADD COLUMN origin TEXT NOT NULL DEFAULT 'discovered'
    CHECK (origin IN ('discovered', 'manual'));

-- What the last image scan of a source found, so the UI can show it and usage
-- can be recomputed (after a mapping changes) without scanning again.
CREATE TABLE source_image (
    source_id  INTEGER NOT NULL REFERENCES source (id) ON DELETE CASCADE,
    registry   TEXT    NOT NULL,
    repository TEXT    NOT NULL,
    tag        TEXT,
    digest     TEXT,
    file       TEXT    NOT NULL,
    origin     TEXT    NOT NULL
);
CREATE INDEX source_image_source ON source_image (source_id);

CREATE TABLE source_skipped (
    source_id INTEGER NOT NULL REFERENCES source (id) ON DELETE CASCADE,
    file      TEXT    NOT NULL,
    value     TEXT    NOT NULL,
    reason    TEXT    NOT NULL
);
CREATE INDEX source_skipped_source ON source_skipped (source_id);

-- Cache of registry lookups, per repository (not per tag).
CREATE TABLE image_resolution (
    registry    TEXT NOT NULL,
    repository  TEXT NOT NULL,
    upstream    TEXT,          -- canonical project repository from the label
    label       TEXT,          -- the label as the registry returned it
    error       TEXT,          -- why there is no label (NULL when there is)
    -- A temporary failure (a rate limit): ask again at the next scan, however
    -- recent this answer is.
    retry       INTEGER NOT NULL DEFAULT 0 CHECK (retry IN (0, 1)),
    resolved_at TEXT NOT NULL,
    PRIMARY KEY (registry, repository)
);

-- An admin's decision about which project an image belongs to. Wins over
-- everything else.
CREATE TABLE image_mapping (
    registry        TEXT NOT NULL,
    repository      TEXT NOT NULL,
    project_repo    TEXT NOT NULL,
    created_by      TEXT NOT NULL,
    created_by_name TEXT NOT NULL DEFAULT '',
    created_at      TEXT NOT NULL,
    PRIMARY KEY (registry, repository)
);
