-- Company career boards the search watches, found by discovery or listed in config.toml.
CREATE TABLE companies (
    ats          TEXT NOT NULL,
    slug         TEXT NOT NULL,
    name         TEXT NOT NULL DEFAULT '',
    origin       TEXT NOT NULL,
    active       BOOLEAN NOT NULL DEFAULT true,
    added_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_checked TIMESTAMPTZ,
    last_jobs    INT NOT NULL DEFAULT 0,
    last_kept    INT NOT NULL DEFAULT 0,
    empty_since  TIMESTAMPTZ,
    error        TEXT,
    PRIMARY KEY (ats, slug)
);

-- Company names from directories, probed once against the ATS job APIs.
CREATE TABLE company_candidates (
    key       TEXT PRIMARY KEY,
    name      TEXT NOT NULL,
    slug_hint TEXT NOT NULL DEFAULT '',
    origin    TEXT NOT NULL,
    probed_at TIMESTAMPTZ,
    found     BOOLEAN NOT NULL DEFAULT false
);

-- Company boards are tracked in `companies`; fetch_runs keeps the job boards only.
DELETE FROM fetch_runs WHERE source LIKE 'Greenhouse:%' OR source LIKE 'Lever:%' OR source LIKE 'Ashby:%';
