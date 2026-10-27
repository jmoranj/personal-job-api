use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Postgres, QueryBuilder};

use crate::filter::Verdict;
use crate::sources::RawJob;

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
                             tags, salary, posted_at, description, score, matched)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
           ON CONFLICT (id) DO UPDATE SET
             last_seen = now(),
             location = EXCLUDED.location,
             eligibility = EXCLUDED.eligibility,
             eligibility_reason = EXCLUDED.eligibility_reason,
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
        "SELECT id, source, title, company, url, location, eligibility, eligibility_reason, tags, salary,
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
