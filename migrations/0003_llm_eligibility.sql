-- Claude decides the jobs the keyword rules leave as "maybe".
-- `eligibility` stays the effective answer (what the dashboard filters on);
-- `rules_eligibility` keeps what the rules said so we know which jobs Claude should check.
ALTER TABLE jobs
  ADD COLUMN rules_eligibility TEXT,
  ADD COLUMN llm_eligibility TEXT,
  ADD COLUMN llm_reason TEXT,
  ADD COLUMN llm_checked_at TIMESTAMPTZ;

UPDATE jobs SET rules_eligibility = eligibility;
ALTER TABLE jobs ALTER COLUMN rules_eligibility SET NOT NULL;

CREATE INDEX jobs_llm_pending ON jobs (score DESC) WHERE rules_eligibility = 'maybe' AND llm_checked_at IS NULL;
