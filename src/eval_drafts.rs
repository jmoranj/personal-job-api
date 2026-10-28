//! Eval for the application-prep agent.
//!
//!   cargo run -- eval drafts sample [n]   pick n open-to-Brazil jobs (default 12) into evals/drafts/jobs.json
//!   cargo run -- eval drafts run          draft each job (cached), grade, write evals/drafts/REPORT.md
//!
//! Grading has two parts:
//! - code checks: word limits, clichés, invented emails, numbers that appear in no source,
//!   and whether the contact's name really is on the page it cites;
//! - a Claude judge with a rubric: claims about João the CV doesn't support, a specific company
//!   detail, coverage of the job's main requirements, honest gaps.
//! Drafts name real people, so they stay out of git (evals/drafts/out/ is ignored); the report doesn't.

use std::collections::BTreeMap;

use anyhow::{bail, Context};
use futures::{stream, StreamExt};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::ai::{Ask, Classifier, Usage};
use crate::prep;

const DIR: &str = "evals/drafts";
const JOBS: &str = "evals/drafts/jobs.json";
const OUT: &str = "evals/drafts/out";

const CLICHES: &[&str] = &[
    "i am excited to apply", "i'm excited to apply", "passionate", "i am writing to express", "dynamic team",
    "fast-paced environment", "team player", "perfect fit", "dream job", "hit the ground running",
];

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
struct EvalJob {
    id: String,
    title: String,
    company: String,
    description: String,
}

pub async fn main(args: &[String], pool: PgPool, client: reqwest::Client) -> anyhow::Result<()> {
    std::fs::create_dir_all(OUT)?;
    match args.first().map(String::as_str) {
        Some("sample") => sample(&pool, args.get(1).and_then(|n| n.parse().ok()).unwrap_or(12)).await,
        Some("run") => run(&pool, client).await,
        _ => bail!("usage: eval drafts sample [n] | eval drafts run"),
    }
}

/// Open-to-Brazil jobs, one per company, spread across match scores so strong and weak fits are both tested.
async fn sample(pool: &PgPool, n: i64) -> anyhow::Result<()> {
    if std::path::Path::new(JOBS).exists() {
        bail!("{JOBS} exists; delete it to sample again");
    }
    let jobs: Vec<EvalJob> = sqlx::query_as(
        "SELECT id, title, company, description FROM (
           SELECT DISTINCT ON (company) id, title, company, description, score
           FROM jobs WHERE eligibility = 'yes' AND length(description) > 800
           ORDER BY company, random()
         ) one_per_company
         ORDER BY ntile(4) OVER (ORDER BY score), random() LIMIT $1",
    )
    .bind(n)
    .fetch_all(pool)
    .await?;
    std::fs::write(JOBS, serde_json::to_string_pretty(&jobs)?)?;
    println!("wrote {} jobs to {JOBS}; next: cargo run -- eval drafts run", jobs.len());
    Ok(())
}

fn out_path(id: &str) -> String {
    format!("{OUT}/{}.json", id.replace(|c: char| !c.is_alphanumeric(), "_"))
}

async fn run(pool: &PgPool, client: reqwest::Client) -> anyhow::Result<()> {
    let jobs: Vec<EvalJob> = serde_json::from_str(&std::fs::read_to_string(JOBS).context("run `eval drafts sample` first")?)?;
    let claude = Classifier::from_env(client.clone())?;
    let cv = prep::load_cv()?;

    // 1. Draft every job that has no cached draft yet.
    let missing: Vec<&EvalJob> = jobs.iter().filter(|j| !std::path::Path::new(&out_path(&j.id)).exists()).collect();
    println!("{} jobs, {} need a draft via {}", jobs.len(), missing.len(), claude.name());
    let claude_ref = &claude;
    let drafted: Vec<(String, anyhow::Result<(Value, f64)>)> = stream::iter(missing)
        .map(|j| async move { (j.id.clone(), prep::draft(pool, claude_ref, &j.id).await) })
        .buffer_unordered(2)
        .collect()
        .await;
    for (id, r) in drafted {
        match r {
            Ok((draft, dollars)) => {
                std::fs::write(out_path(&id), serde_json::to_string_pretty(&json!({"draft": draft, "dollars": dollars}))?)?;
            }
            Err(e) => eprintln!("drafting {id} failed: {e:#}"),
        }
    }

    // 2. Grade.
    let mut rows = Vec::new();
    let mut judge_usage = Usage::default();
    for j in &jobs {
        let Ok(raw) = std::fs::read_to_string(out_path(&j.id)) else { continue };
        let saved: Value = serde_json::from_str(&raw)?;
        let draft = &saved["draft"];
        let checks = code_checks(&client, draft, &cv, &j.description).await;
        let (verdict, u) = judge(&claude, j, draft, &cv).await.unwrap_or_else(|e| {
            eprintln!("judging {} failed: {e:#}", j.id);
            (Value::Null, Usage::default())
        });
        judge_usage.add(u);
        rows.push(Row { job: j.clone(), checks, judge: verdict, draft_dollars: saved["dollars"].as_f64().unwrap_or(0.0) });
    }
    if rows.is_empty() {
        bail!("no drafts to grade");
    }
    let report = report(&rows, judge_usage, claude.name());
    std::fs::write(format!("{DIR}/REPORT.md"), &report)?;
    println!("{report}\nwritten to {DIR}/REPORT.md");
    Ok(())
}

struct Row {
    job: EvalJob,
    checks: BTreeMap<&'static str, Check>,
    judge: Value,
    draft_dollars: f64,
}

/// A code check: passed, failed with a note, or not applicable to this draft.
#[derive(Debug, Clone)]
enum Check {
    Pass,
    Fail(String),
    Na,
}

fn words(s: &str) -> usize {
    s.split_whitespace().count()
}

async fn code_checks(client: &reqwest::Client, d: &Value, cv: &str, posting: &str) -> BTreeMap<&'static str, Check> {
    let text = |k: &str| d[k].as_str().unwrap_or("").to_string();
    let (cover, outreach, follow) = (text("cover_letter"), text("outreach_message"), text("follow_up"));
    let sendable = format!("{cover}\n{outreach}\n{follow}");
    let mut c = BTreeMap::new();

    let limit = |s: &str, max: usize| if words(s) <= max { Check::Pass } else { Check::Fail(format!("{} words > {max}", words(s))) };
    c.insert("cover letter ≤180 words", limit(&cover, 180));
    let grounded = d["self_check"].as_array().map(|a| a.iter().all(|x| !x["cv_line"].as_str().unwrap_or("").trim().is_empty()));
    c.insert("self-check complete", match grounded {
        Some(true) => Check::Pass,
        Some(false) => Check::Fail("a claim has no CV line".into()),
        None => Check::Na,
    });
    c.insert("message ≤90 words", limit(&outreach, 90));
    c.insert("follow-up ≤60 words", limit(&follow, 60));

    let lower = sendable.to_lowercase();
    let hits: Vec<&str> = CLICHES.iter().copied().filter(|p| lower.contains(p)).collect();
    c.insert("no clichés", if hits.is_empty() { Check::Pass } else { Check::Fail(hits.join(", ")) });

    // Any email in what he'd send must be his own; the agent must never guess one.
    let email = Regex::new(r"[\w.+-]+@[\w-]+\.[\w.]+").unwrap();
    let foreign: Vec<String> = email
        .find_iter(&format!("{sendable}\n{}", d["contact"]["how_to_reach"].as_str().unwrap_or("")))
        .map(|m| m.as_str().to_string())
        .filter(|e| !cv.contains(e.as_str()))
        .collect();
    let page = page_text(client, d["contact"]["source_url"].as_str()).await.unwrap_or_default();
    let posting_lower = posting.to_lowercase();
    let guessed: Vec<String> = foreign
        .into_iter()
        .filter(|e| !page.contains(&e.to_lowercase()) && !posting_lower.contains(&e.to_lowercase()))
        .collect();
    c.insert("no guessed emails", if guessed.is_empty() { Check::Pass } else { Check::Fail(guessed.join(", ")) });

    // Every number he'd send must come from the CV, the posting, or the agent's own research notes.
    let sources = format!("{cv}\n{posting}\n{}", d["company_notes"].as_str().unwrap_or(""));
    let number = Regex::new(r"\$?\d[\d,.]*%?").unwrap();
    let norm = |s: &str| s.trim_end_matches(['.', ',']).replace(',', "");
    let src_norm = sources.replace(',', "");
    let unknown: Vec<String> = number
        .find_iter(&sendable)
        .map(|m| norm(m.as_str()))
        .filter(|n| n.chars().filter(|c| c.is_ascii_digit()).count() >= 2 && !src_norm.contains(n.as_str()))
        .collect();
    c.insert("numbers traceable", if unknown.is_empty() { Check::Pass } else { Check::Fail(unknown.join(", ")) });

    let highlights = d["highlights"].as_array().map(Vec::len).unwrap_or(0);
    c.insert("2–3 highlights", if (2..=3).contains(&highlights) { Check::Pass } else { Check::Fail(format!("{highlights}")) });

    c.insert("contact on its source page", contact_check(client, &d["contact"]).await);
    c
}

/// Lowercased page text, or None when the page can't be read by a plain HTTP client.
async fn page_text(client: &reqwest::Client, url: Option<&str>) -> Option<String> {
    let url = url.filter(|u| u.starts_with("http"))?;
    let r = client.get(url).send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.text().await.ok().map(|t| t.to_lowercase())
}

/// A named contact must appear (by last name) on the page the agent cites. LinkedIn blocks bots
/// and some pages render with JavaScript, so those count as unverifiable (n/a), not as failures.
async fn contact_check(client: &reqwest::Client, contact: &Value) -> Check {
    let Some(name) = contact["name"].as_str().filter(|n| !n.trim().is_empty()) else { return Check::Na };
    let Some(url) = contact["source_url"].as_str().filter(|u| u.starts_with("http")) else {
        return Check::Fail("named without a source".into());
    };
    if url.contains("linkedin.com") {
        return Check::Na;
    }
    let last = name.split_whitespace().last().unwrap_or(name).to_lowercase();
    match page_text(client, Some(url)).await {
        Some(body) if body.contains(&last) => Check::Pass,
        Some(body) if body.len() < 5000 => Check::Na,
        Some(_) => Check::Fail("name not on the cited page".into()),
        None => Check::Na,
    }
}

const JUDGE: &str = "You grade an application draft written for a job seeker. You get his CV, the job posting, \
and the draft (cover letter, outreach message, follow-up, highlights, gaps). Be strict and literal.

- unsupported_claims: every statement about the candidate in the cover letter, outreach or follow-up that the CV \
does not support (invented skills, employers, numbers, durations, responsibilities). Quote each. Empty if none.
- company_specific: true if the cover letter mentions a concrete, checkable fact about this company beyond the \
posting's own text (product, partnership, customer, blog post); company_detail quotes it.
- requirements_coverage 1-5: how well the cover letter connects the candidate's real experience to the job's \
main requirements (5 = the top requirements are each addressed with a concrete CV example).
- gaps_honest: true if the draft's gaps list names the important requirements the CV clearly lacks, and the \
letter doesn't claim them.
- would_send: true if a careful candidate could send the cover letter with only light edits.
- note: one sentence on the biggest problem, or \"none\".";

fn judge_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "unsupported_claims": {"type": "array", "items": {"type": "string"}},
            "company_specific": {"type": "boolean"},
            "company_detail": {"type": "string"},
            "requirements_coverage": {"type": "integer", "enum": [1, 2, 3, 4, 5]},
            "gaps_honest": {"type": "boolean"},
            "would_send": {"type": "boolean"},
            "note": {"type": "string"}
        },
        "required": ["unsupported_claims", "company_specific", "company_detail", "requirements_coverage",
                     "gaps_honest", "would_send", "note"],
        "additionalProperties": false
    })
}

async fn judge(claude: &Classifier, j: &EvalJob, d: &Value, cv: &str) -> anyhow::Result<(Value, Usage)> {
    let draft = json!({
        "cover_letter": d["cover_letter"], "outreach_message": d["outreach_message"], "follow_up": d["follow_up"],
        "highlights": d["highlights"], "gaps": d["gaps"]
    });
    let prompt = format!(
        "CV\n{cv}\n\n---\nJOB POSTING\n{} at {}\n\n{}\n\n---\nDRAFT\n{}",
        j.title,
        j.company,
        j.description,
        serde_json::to_string_pretty(&draft)?
    );
    claude.ask(&Ask { system: JUDGE, prompt: &prompt, schema: judge_schema(), effort: "medium", web: false }).await
}

fn report(rows: &[Row], judge_usage: Usage, backend: &str) -> String {
    let n = rows.len();
    let mut r = format!("# Application-draft eval — {}\n\n", chrono::Utc::now().format("%Y-%m-%d"));
    r.push_str(&format!(
        "{n} open-to-Brazil jobs, one per company. Drafts and judge via {backend}. \
         The judge is also Claude, so its scores are a second opinion, not ground truth; \
         the code checks are objective.\n\n"
    ));

    r.push_str("## Code checks\n\n| Check | Passed | Failures |\n|---|---|---|\n");
    let names: Vec<&str> = rows[0].checks.keys().copied().collect();
    for name in names {
        let mut pass = 0;
        let mut applicable = 0;
        let mut fails = Vec::new();
        for row in rows {
            match &row.checks[name] {
                Check::Pass => {
                    pass += 1;
                    applicable += 1;
                }
                Check::Fail(why) => {
                    applicable += 1;
                    fails.push(format!("{}: {why}", row.job.company));
                }
                Check::Na => {}
            }
        }
        let na = n - applicable;
        r.push_str(&format!(
            "| {name} | {pass}/{applicable}{} | {} |\n",
            if na > 0 { format!(" ({na} n/a)") } else { String::new() },
            if fails.is_empty() { "—".into() } else { fails.join("; ") }
        ));
    }

    let judged: Vec<&Row> = rows.iter().filter(|r| !r.judge.is_null()).collect();
    let jn = judged.len().max(1) as f64;
    let count = |f: &dyn Fn(&Value) -> bool| judged.iter().filter(|r| f(&r.judge)).count();
    let clean = count(&|v| v["unsupported_claims"].as_array().is_some_and(|a| a.is_empty()));
    let specific = count(&|v| v["company_specific"].as_bool() == Some(true));
    let honest = count(&|v| v["gaps_honest"].as_bool() == Some(true));
    let send = count(&|v| v["would_send"].as_bool() == Some(true));
    let coverage: f64 = judged.iter().filter_map(|r| r.judge["requirements_coverage"].as_f64()).sum::<f64>() / jn;
    r.push_str(&format!(
        "\n## Judge rubric ({} drafts)\n\n| Criterion | Result |\n|---|---|\n\
         | No unsupported claims about João | {clean}/{} |\n\
         | Specific, checkable company detail | {specific}/{} |\n\
         | Requirements coverage (1–5, mean) | {coverage:.1} |\n\
         | Gaps listed honestly | {honest}/{} |\n\
         | Sendable with light edits | {send}/{} |\n",
        judged.len(), judged.len(), judged.len(), judged.len(), judged.len()
    ));

    r.push_str("\n## Per draft\n\n| Job | Fit | Code checks | Unsupported claims | Judge note |\n|---|---|---|---|---|\n");
    for row in rows {
        let passed = row.checks.values().filter(|c| matches!(c, Check::Pass)).count();
        let applicable = row.checks.values().filter(|c| !matches!(c, Check::Na)).count();
        let claims = row.judge["unsupported_claims"]
            .as_array()
            .map(|a| if a.is_empty() { "none".to_string() } else { a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" · ") })
            .unwrap_or_else(|| "not judged".into());
        r.push_str(&format!(
            "| {} — {} | {} | {passed}/{applicable} | {} | {} |\n",
            row.job.title.replace('|', "/"),
            row.job.company,
            fit(&row.job),
            claims.replace('|', "/"),
            row.judge["note"].as_str().unwrap_or("").replace('|', "/")
        ));
    }

    let draft_cost: f64 = rows.iter().map(|r| r.draft_dollars).sum();
    r.push_str(&format!(
        "\n## Cost\n\nDrafts: ${draft_cost:.2} ({:.2} per draft). Judge: ${:.2}. At API list price; through Claude Code this uses subscription limits instead.\n",
        draft_cost / n as f64,
        judge_usage.dollars
    ));
    r
}

fn fit(job: &EvalJob) -> String {
    std::fs::read_to_string(out_path(&job.id))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v["draft"]["fit"]["score"].as_i64())
        .map(|s| format!("{s}/5"))
        .unwrap_or_else(|| "—".into())
}
