CREATE TABLE jobs (
    id                 TEXT PRIMARY KEY,
    source             TEXT NOT NULL,
    title              TEXT NOT NULL,
    company            TEXT NOT NULL,
    url                TEXT NOT NULL,
    location           TEXT NOT NULL DEFAULT '',
    eligibility        TEXT NOT NULL,
    eligibility_reason TEXT NOT NULL,
    tags               TEXT[] NOT NULL DEFAULT '{}',
    salary             TEXT,
    posted_at          TIMESTAMPTZ,
    description        TEXT NOT NULL DEFAULT '',
    score              INT NOT NULL DEFAULT 0,
    matched            TEXT[] NOT NULL DEFAULT '{}',
    status             TEXT NOT NULL DEFAULT 'new',
    notes              TEXT NOT NULL DEFAULT '',
    first_seen         TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen          TIMESTAMPTZ NOT NULL DEFAULT now(),
    status_changed_at  TIMESTAMPTZ
);
CREATE INDEX jobs_status_idx ON jobs (status);
CREATE INDEX jobs_score_idx ON jobs (score DESC);

CREATE TABLE fetch_runs (
    id          BIGSERIAL PRIMARY KEY,
    source      TEXT NOT NULL,
    started_at  TIMESTAMPTZ NOT NULL,
    finished_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    fetched     INT NOT NULL DEFAULT 0,
    kept        INT NOT NULL DEFAULT 0,
    error       TEXT
);
