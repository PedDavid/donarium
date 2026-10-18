-- When a project's repository was last asked for its FUNDING.yml.
ALTER TABLE project ADD COLUMN funding_checked_at TEXT;
