-- Application drafts the prep agent writes for a job; João reviews and sends them himself.
CREATE TABLE applications (
  job_id     TEXT PRIMARY KEY REFERENCES jobs(id) ON DELETE CASCADE,
  state      TEXT NOT NULL,          -- preparing | ready | failed
  draft      JSONB,
  error      TEXT,
  backend    TEXT NOT NULL DEFAULT '',
  dollars    DOUBLE PRECISION NOT NULL DEFAULT 0,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
