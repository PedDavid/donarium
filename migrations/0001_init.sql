-- Repositories to scan for dependencies. Entered by admins in the UI.
CREATE TABLE source (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    provider         TEXT    NOT NULL CHECK (provider IN ('github', 'gitlab')),
    repo             TEXT    NOT NULL,
    weight           REAL    NOT NULL CHECK (weight > 0 AND weight <= 100),
    scan_method      TEXT    NOT NULL DEFAULT 'auto'
                     CHECK (scan_method IN ('auto', 'dependency-graph', 'sbom', 'lockfiles')),
    enabled          INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    last_scan_at     TEXT,
    last_scan_status TEXT,
    created_by       TEXT    NOT NULL,
    created_by_name  TEXT    NOT NULL DEFAULT '',
    created_at       TEXT    NOT NULL,
    updated_by       TEXT    NOT NULL,
    updated_by_name  TEXT    NOT NULL DEFAULT '',
    updated_at       TEXT    NOT NULL
);

-- GitHub and GitLab names are case-insensitive.
CREATE UNIQUE INDEX source_repo ON source (provider, repo COLLATE NOCASE);

-- Upstream projects, discovered by scans. Never created by hand.
CREATE TABLE project (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    name          TEXT NOT NULL UNIQUE,   -- URL slug, e.g. tokio-rs-tokio
    repo          TEXT NOT NULL UNIQUE,   -- canonical, e.g. github.com/tokio-rs/tokio
    first_seen_at TEXT NOT NULL,
    last_seen_at  TEXT NOT NULL
);

-- Which sources use which project, and how. Rewritten by each scan.
CREATE TABLE usage (
    source_id  INTEGER NOT NULL REFERENCES source (id) ON DELETE CASCADE,
    project_id INTEGER NOT NULL REFERENCES project (id) ON DELETE CASCADE,
    kind       TEXT    NOT NULL CHECK (kind IN ('direct', 'transitive')),
    PRIMARY KEY (source_id, project_id)
);

CREATE TABLE project_funding (
    project_id INTEGER NOT NULL REFERENCES project (id) ON DELETE CASCADE,
    platform   TEXT    NOT NULL,
    handle     TEXT    NOT NULL,
    url        TEXT,
    origin     TEXT    NOT NULL DEFAULT 'discovered' CHECK (origin IN ('discovered', 'manual')),
    PRIMARY KEY (project_id, platform, handle)
);
