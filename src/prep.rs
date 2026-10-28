//! Application-prep agent: reads a job and the CV, looks the company up on the web, and drafts
//! a cover letter, a short message to a hiring manager, and a follow-up. João reviews and sends.

use anyhow::Context;
use serde_json::json;
use sqlx::PgPool;

use crate::ai::{Ask, Classifier};

const SYSTEM: &str = "You help a software engineer living in Brazil apply to a remote job. \
You get the job posting and his CV. Use web search and fetch to learn about the company \
(product, stage, recent news, engineering blog) and to find who hires for this role \
(engineering manager, head of engineering, CTO or founder at a small company).

Rules:
- Use only facts from the CV. Never invent experience, numbers, employers or skills. If the job asks for \
something the CV doesn't show, list it under gaps; don't hide it.
- Pick the 2-3 CV experiences that best match what this job needs, and say why in the job's own terms.
- Cover letter: under 180 words, plain and specific, no clichés (\"I am excited to apply\", \"passionate\"). \
Open with what he'd do for them, mention one concrete thing about the company you found, end with a simple ask.
- Outreach message: under 90 words, for LinkedIn or email, to the person in contact. One specific hook, \
one proof point, one ask (a short call or to flag his application).
- Contact: only a real person you found on a public page, with the page URL. Never guess an email address. \
If you can't find someone, leave name and source_url null and say where to look instead.
- Follow-up: under 60 words, to send 5-7 days after applying if there's no reply.
- checks: things he must verify before sending (e.g. a claim you weren't sure of, a contact that may be outdated).
- Write in English.";

fn schema() -> serde_json::Value {
    let text = json!({"type": "string"});
    let maybe = json!({"type": ["string", "null"]});
    json!({
        "type": "object",
        "properties": {
            "fit": {
                "type": "object",
                "properties": {
                    "score": {"type": "integer", "enum": [1, 2, 3, 4, 5]},
                    "summary": text
                },
                "required": ["score", "summary"],
                "additionalProperties": false
            },
            "company_notes": text,
            "highlights": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {"from_cv": text, "why_it_matters": text},
                    "required": ["from_cv", "why_it_matters"],
                    "additionalProperties": false
                }
            },
            "gaps": {"type": "array", "items": text},
            "cover_letter": text,
            "contact": {
                "type": "object",
                "properties": {"name": maybe, "role": maybe, "source_url": maybe, "how_to_reach": text},
                "required": ["name", "role", "source_url", "how_to_reach"],
                "additionalProperties": false
            },
            "outreach_message": text,
            "follow_up": text,
            "checks": {"type": "array", "items": text},
            "sources": {"type": "array", "items": text}
        },
        "required": ["fit", "company_notes", "highlights", "gaps", "cover_letter", "contact",
                     "outreach_message", "follow_up", "checks", "sources"],
        "additionalProperties": false
    })
}

#[derive(sqlx::FromRow)]
struct JobText {
    title: String,
    company: String,
    url: String,
    location: String,
    description: String,
}

pub fn load_cv() -> anyhow::Result<String> {
    let path = std::env::var("CV_PATH").unwrap_or_else(|_| "cv.md".into());
    std::fs::read_to_string(&path).with_context(|| format!("reading the CV at {path}; set CV_PATH or create cv.md"))
}

/// Drafts the application and stores it; failures are stored too, so the dashboard can show them.
pub async fn prepare(pool: &PgPool, claude: &Classifier, job_id: &str) {
    let result = draft(pool, claude, job_id).await;
    let saved = match result {
        Ok((draft, dollars)) => sqlx::query(
            "UPDATE applications SET state = 'ready', draft = $2, error = NULL, dollars = $3, updated_at = now()
             WHERE job_id = $1",
        )
        .bind(job_id)
        .bind(draft)
        .bind(dollars)
        .execute(pool)
        .await,
        Err(e) => {
            tracing::warn!("preparing {job_id} failed: {e:#}");
            sqlx::query("UPDATE applications SET state = 'failed', error = $2, updated_at = now() WHERE job_id = $1")
                .bind(job_id)
                .bind(format!("{e:#}"))
                .execute(pool)
                .await
        }
    };
    if let Err(e) = saved {
        tracing::warn!("saving the draft for {job_id} failed: {e}");
    }
}

async fn draft(pool: &PgPool, claude: &Classifier, job_id: &str) -> anyhow::Result<(serde_json::Value, f64)> {
    let cv = load_cv()?;
    let job: JobText = sqlx::query_as("SELECT title, company, url, location, description FROM jobs WHERE id = $1")
        .bind(job_id)
        .fetch_one(pool)
        .await
        .context("loading the job")?;
    let prompt = format!(
        "JOB POSTING\nTitle: {}\nCompany: {}\nLocation: {}\nURL: {}\n\n{}\n\n---\n\nCV\n{cv}",
        job.title, job.company, job.location, job.url, job.description
    );
    let started = std::time::Instant::now();
    let (v, usage) = claude
        .ask(&Ask { system: SYSTEM, prompt: &prompt, schema: schema(), effort: "medium", web: true })
        .await?;
    tracing::info!(
        "prepared {job_id} in {}s (${:.2} at list price)",
        started.elapsed().as_secs(),
        usage.dollars
    );
    Ok((v, usage.dollars))
}

/// Marks the job as being prepared. Returns false if a draft is already being written.
pub async fn start(pool: &PgPool, job_id: &str, backend: &str) -> sqlx::Result<bool> {
    let r = sqlx::query(
        "INSERT INTO applications (job_id, state, backend) VALUES ($1, 'preparing', $2)
         ON CONFLICT (job_id) DO UPDATE SET state = 'preparing', error = NULL, backend = $2, updated_at = now()
         WHERE applications.state <> 'preparing'",
    )
    .bind(job_id)
    .bind(backend)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

#[derive(Debug, serde::Serialize, sqlx::FromRow)]
pub struct Application {
    pub job_id: String,
    pub state: String,
    pub draft: Option<serde_json::Value>,
    pub error: Option<String>,
    pub backend: String,
    pub dollars: f64,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

pub async fn get(pool: &PgPool, job_id: &str) -> sqlx::Result<Option<Application>> {
    sqlx::query_as("SELECT job_id, state, draft, error, backend, dollars, updated_at FROM applications WHERE job_id = $1")
        .bind(job_id)
        .fetch_optional(pool)
        .await
}
