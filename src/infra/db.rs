use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder};

use crate::filter::Verdict;
use crate::sources::{Board, RawJob};

pub const STATUSES: &[&str] = &["new", "saved", "applied", "interviewing", "offer", "rejected", "dismissed"];

#[derive(Debug, Serialize, FromRow)]
pub struct Job {
    pub id: String,
    pub source: String,
    pub title: String,
    pub company: String,
    pub url: String,
    pub location: String,
    pub eligibility: String,
    pub eligibility_reason: String,
    /// "claude" when Claude decided the eligibility, else "rules".
    pub checked_by: String,
    pub tags: Vec<String>,
    pub salary: Option<String>,
    pub posted_at: Option<DateTime<Utc>>,
    pub description: String,
    pub score: i32,
    pub matched: Vec<String>,
    pub status: String,
    pub notes: String,
    pub first_seen: DateTime<Utc>,
    pub status_changed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, Deserialize)]
pub struct JobFilter {
    pub status: Option<String>,
    pub eligibility: Option<String>,
    pub q: Option<String>,
    pub limit: Option<i64>,
}

/// Same role at the same company from two boards collapses into one row.
pub fn job_key(company: &str, title: &str) -> String {
    let norm = |s: &str| s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect::<String>();
    format!("{}|{}", norm(company), norm(title))
}

pub async fn upsert(pool: &PgPool, job: &RawJob, v: &Verdict) -> sqlx::Result<()> {
    sqlx::query(
        r#"INSERT INTO jobs (id, source, title, company, url, location, eligibility, eligibility_reason,
                             tags, salary, posted_at, description, score, matched, rules_eligibility)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$7)
           ON CONFLICT (id) DO UPDATE SET
             last_seen = now(),
             location = EXCLUDED.location,
             rules_eligibility = EXCLUDED.eligibility,
             -- Keep Claude's answer while the rules still can't decide.
             eligibility = CASE WHEN EXCLUDED.eligibility = 'maybe' AND jobs.llm_eligibility IS NOT NULL
                                THEN jobs.llm_eligibility ELSE EXCLUDED.eligibility END,
             eligibility_reason = CASE WHEN EXCLUDED.eligibility = 'maybe' AND jobs.llm_eligibility IS NOT NULL
                                THEN jobs.llm_reason ELSE EXCLUDED.eligibility_reason END,
             tags = EXCLUDED.tags,
             salary = COALESCE(EXCLUDED.salary, jobs.salary),
             posted_at = COALESCE(EXCLUDED.posted_at, jobs.posted_at),
             description = EXCLUDED.description,
             score = EXCLUDED.score,
             matched = EXCLUDED.matched
           WHERE jobs.source = EXCLUDED.source"#,
    )
    .bind(job_key(&job.company, &job.title))
    .bind(&job.source)
    .bind(&job.title)
    .bind(&job.company)
    .bind(&job.url)
    .bind(&job.location)
    .bind(v.eligibility.as_str())
    .bind(&v.reason)
    .bind(&job.tags)
    .bind(&job.salary)
    .bind(job.posted_at)
    .bind(&job.description)
    .bind(v.score)
    .bind(&v.matched)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list(pool: &PgPool, f: &JobFilter) -> sqlx::Result<Vec<Job>> {
    let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(
        "SELECT id, source, title, company, url, location, eligibility, eligibility_reason,
                CASE WHEN rules_eligibility = 'maybe' AND llm_eligibility IS NOT NULL THEN 'claude' ELSE 'rules' END AS checked_by,
                tags, salary,
                posted_at, description, score, matched, status, notes, first_seen, status_changed_at
         FROM jobs WHERE 1=1",
    );
    match f.status.as_deref().filter(|s| !s.is_empty()) {
        Some("all") => {}
        Some(s) => {
            qb.push(" AND status = ").push_bind(s.to_string());
        }
        None => {
            qb.push(" AND status = 'new'");
        }
    }
    match f.eligibility.as_deref().filter(|e| !e.is_empty()) {
        Some("all") => {}
        Some(e) => {
            qb.push(" AND eligibility = ").push_bind(e.to_string());
        }
        None => {
            qb.push(" AND eligibility <> 'no'");
        }
    }
    if let Some(q) = f.q.as_deref().filter(|q| !q.trim().is_empty()) {
        let like = format!("%{}%", q.trim());
        qb.push(" AND (title ILIKE ").push_bind(like.clone());
        qb.push(" OR company ILIKE ").push_bind(like.clone());
        qb.push(" OR description ILIKE ").push_bind(like).push(")");
    }
    qb.push(" ORDER BY score DESC, posted_at DESC NULLS LAST LIMIT ")
        .push_bind(f.limit.unwrap_or(1000).clamp(1, 1000));
    qb.build_query_as().fetch_all(pool).await
}

/// A job the rules called "maybe" that Claude hasn't looked at yet.
#[derive(Debug, FromRow)]
pub struct Pending {
    pub id: String,
    pub title: String,
    pub company: String,
    pub location: String,
    pub description: String,
}

/// Best-scoring first, so the jobs most worth seeing get decided first.
pub async fn llm_pending(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<Pending>> {
    sqlx::query_as(
        "SELECT id, title, company, location, description FROM jobs
         WHERE rules_eligibility = 'maybe' AND llm_checked_at IS NULL AND status = 'new'
         ORDER BY score DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Stores Claude's answer. `None` records a failed check so it isn't retried every run.
pub async fn set_llm(pool: &PgPool, id: &str, verdict: Option<(&str, &str)>) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE jobs SET llm_checked_at = now(), llm_eligibility = $2, llm_reason = $3,
           eligibility = CASE WHEN $2::text IS NOT NULL AND rules_eligibility = 'maybe' THEN $2 ELSE eligibility END,
           eligibility_reason = CASE WHEN $2::text IS NOT NULL AND rules_eligibility = 'maybe' THEN $3 ELSE eligibility_reason END
         WHERE id = $1",
    )
    .bind(id)
    .bind(verdict.map(|v| v.0))
    .bind(verdict.map(|v| v.1))
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Serialize, FromRow)]
pub struct LlmSummary {
    pub checked: i64,
    pub pending: i64,
    pub opened: i64,
    pub closed: i64,
}

pub async fn llm_summary(pool: &PgPool) -> sqlx::Result<LlmSummary> {
    sqlx::query_as(
        "SELECT count(*) FILTER (WHERE llm_eligibility IS NOT NULL) AS checked,
                count(*) FILTER (WHERE llm_checked_at IS NULL AND status = 'new') AS pending,
                count(*) FILTER (WHERE llm_eligibility = 'yes') AS opened,
                count(*) FILTER (WHERE llm_eligibility = 'no') AS closed
         FROM jobs WHERE rules_eligibility = 'maybe'",
    )
    .fetch_one(pool)
    .await
}

pub async fn update(pool: &PgPool, id: &str, status: Option<&str>, notes: Option<&str>) -> sqlx::Result<u64> {
    let r = sqlx::query(
        "UPDATE jobs SET
           status_changed_at = CASE WHEN $2::text IS NOT NULL AND $2 <> status THEN now() ELSE status_changed_at END,
           status = COALESCE($2, status),
           notes = COALESCE($3, notes)
         WHERE id = $1",
    )
    .bind(id)
    .bind(status)
    .bind(notes)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

#[derive(Debug, Serialize, FromRow)]
pub struct Count {
    pub key: String,
    pub n: i64,
}

#[derive(Debug, Serialize, FromRow)]
pub struct SourceRun {
    pub source: String,
    pub finished_at: DateTime<Utc>,
    pub fetched: i32,
    pub kept: i32,
    pub error: Option<String>,
}

pub async fn status_counts(pool: &PgPool) -> sqlx::Result<Vec<Count>> {
    sqlx::query_as("SELECT status AS key, count(*) AS n FROM jobs WHERE eligibility <> 'no' GROUP BY status")
        .fetch_all(pool)
        .await
}

pub async fn new_today(pool: &PgPool) -> sqlx::Result<i64> {
    sqlx::query_scalar("SELECT count(*) FROM jobs WHERE eligibility <> 'no' AND first_seen > now() - interval '24 hours'")
        .fetch_one(pool)
        .await
}

pub async fn latest_runs(pool: &PgPool) -> sqlx::Result<Vec<SourceRun>> {
    sqlx::query_as(
        "SELECT DISTINCT ON (source) source, finished_at, fetched, kept, error
         FROM fetch_runs ORDER BY source, finished_at DESC",
    )
    .fetch_all(pool)
    .await
}

pub async fn record_run(pool: &PgPool, source: &str, started: DateTime<Utc>, fetched: usize, kept: usize, error: Option<String>) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO fetch_runs (source, started_at, fetched, kept, error) VALUES ($1,$2,$3,$4,$5)")
        .bind(source)
        .bind(started)
        .bind(fetched as i32)
        .bind(kept as i32)
        .bind(error)
        .execute(pool)
        .await?;
    Ok(())
}

// ---- Company boards -------------------------------------------------------

/// URLs and descriptions of every stored job, for harvesting ATS links.
pub async fn job_link_text(pool: &PgPool) -> sqlx::Result<String> {
    sqlx::query_scalar("SELECT coalesce(string_agg(url || ' ' || description, ' '), '') FROM jobs")
        .fetch_one(pool)
        .await
}

/// Returns true when the board was new.
pub async fn add_company(pool: &PgPool, ats: &str, slug: &str, name: &str, origin: &str) -> sqlx::Result<bool> {
    let r = sqlx::query(
        "INSERT INTO companies (ats, slug, name, origin) VALUES ($1, $2, $3, $4)
         ON CONFLICT (ats, slug) DO UPDATE SET name = CASE WHEN companies.name = '' THEN EXCLUDED.name ELSE companies.name END
         RETURNING (xmax = 0) AS inserted",
    )
    .bind(ats)
    .bind(slug)
    .bind(name)
    .bind(origin)
    .fetch_one(pool)
    .await?;
    Ok(sqlx::Row::get(&r, "inserted"))
}

pub fn candidate_key(name: &str) -> String {
    name.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Returns true when the candidate was new.
pub async fn add_candidate(pool: &PgPool, name: &str, slug_hint: &str, origin: &str) -> sqlx::Result<bool> {
    let key = candidate_key(name);
    if key.len() < 2 {
        return Ok(false);
    }
    let r = sqlx::query("INSERT INTO company_candidates (key, name, slug_hint, origin) VALUES ($1, $2, $3, $4) ON CONFLICT (key) DO NOTHING")
        .bind(key)
        .bind(name)
        .bind(slug_hint)
        .bind(origin)
        .execute(pool)
        .await?;
    Ok(r.rows_affected() == 1)
}

#[derive(Debug, FromRow)]
pub struct Candidate {
    pub key: String,
    pub name: String,
    pub slug_hint: String,
}

pub async fn candidates_to_probe(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<Candidate>> {
    sqlx::query_as(
        "SELECT key, name, slug_hint FROM company_candidates
         WHERE probed_at IS NULL OR (NOT found AND probed_at < now() - interval '30 days')
         ORDER BY probed_at NULLS FIRST LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn mark_probed(pool: &PgPool, key: &str, found: bool) -> sqlx::Result<()> {
    sqlx::query("UPDATE company_candidates SET probed_at = now(), found = $2 WHERE key = $1")
        .bind(key)
        .bind(found)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn active_boards(pool: &PgPool) -> sqlx::Result<Vec<Board>> {
    let rows: Vec<(String, String, String)> = sqlx::query_as("SELECT ats, slug, name FROM companies WHERE active ORDER BY ats, slug")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(ats, slug, name)| Board { ats, slug, name }).collect())
}

/// Records one fetch of a board. Boards with no matching roles for 30 days stop being watched.
pub async fn record_board_check(pool: &PgPool, b: &Board, jobs: usize, kept: usize, error: Option<String>) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE companies SET
           last_checked = now(),
           last_jobs = $3,
           last_kept = $4,
           error = $5,
           empty_since = CASE WHEN $4 > 0 THEN NULL ELSE coalesce(empty_since, now()) END,
           active = NOT ($4 = 0 AND coalesce(empty_since, now()) < now() - interval '30 days')
         WHERE ats = $1 AND slug = $2",
    )
    .bind(&b.ats)
    .bind(&b.slug)
    .bind(jobs as i32)
    .bind(kept as i32)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Serialize, FromRow)]
pub struct Company {
    pub ats: String,
    pub slug: String,
    pub name: String,
    pub origin: String,
    pub active: bool,
    pub added_at: DateTime<Utc>,
    pub last_checked: Option<DateTime<Utc>>,
    pub last_jobs: i32,
    pub last_kept: i32,
    pub error: Option<String>,
}

pub async fn list_companies(pool: &PgPool) -> sqlx::Result<Vec<Company>> {
    sqlx::query_as(
        "SELECT ats, slug, name, origin, active, added_at, last_checked, last_jobs, last_kept, error
         FROM companies ORDER BY active DESC, last_kept DESC, name, slug",
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug, Default, Serialize, FromRow)]
pub struct CompanySummary {
    pub watched: i64,
    pub with_matches: i64,
    pub failing: i64,
    pub candidates_left: i64,
}

pub async fn company_summary(pool: &PgPool) -> sqlx::Result<CompanySummary> {
    sqlx::query_as(
        "SELECT
           (SELECT count(*) FROM companies WHERE active) AS watched,
           (SELECT count(*) FROM companies WHERE active AND last_kept > 0) AS with_matches,
           (SELECT count(*) FROM companies WHERE active AND error IS NOT NULL) AS failing,
           (SELECT count(*) FROM company_candidates WHERE probed_at IS NULL) AS candidates_left",
    )
    .fetch_one(pool)
    .await
}
