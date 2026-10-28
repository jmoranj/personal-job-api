//! MCP server over stdio, so Claude (Claude Code, Claude Desktop) can search and update Job Radar.
//!
//!   claude mcp add job-radar -- /path/to/job-radar mcp
//!
//! Speaks JSON-RPC 2.0, one message per line. Logs go to stderr; stdout carries only protocol messages.

use serde_json::{json, Value};
use sqlx::PgPool;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::infra::db;
use crate::prep;

const PROTOCOL: &str = "2025-06-18";

fn tools() -> Value {
    json!([
        {
            "name": "search_jobs",
            "description": "Search Job Radar's remote software/AI jobs. Defaults to new jobs open to Brazil or unclear, best match first. Returns id, title, company, location, eligibility, match score and matched skills.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Words to find in title, company or description"},
                    "status": {"type": "string", "enum": ["new", "saved", "applied", "interviewing", "offer", "rejected", "dismissed", "all"], "description": "Default: new"},
                    "eligibility": {"type": "string", "enum": ["yes", "maybe", "no", "all"], "description": "Brazil eligibility. Default: yes and maybe"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "Default: 15"}
                },
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "get_job",
            "description": "Full details of one job by id: description, eligibility reason, notes, and the application draft if one was prepared.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "update_job",
            "description": "Change a job's status (e.g. save it, mark it applied) and/or replace its notes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "status": {"type": "string", "enum": ["new", "saved", "applied", "interviewing", "offer", "rejected", "dismissed"]},
                    "notes": {"type": "string"}
                },
                "required": ["id"],
                "additionalProperties": false
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true}
        },
        {
            "name": "pipeline",
            "description": "Overview of the job search: counts by status, jobs found in the last 24 hours, and applied jobs that are due a follow-up (applied 6+ days ago).",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
            "annotations": {"readOnlyHint": true}
        }
    ])
}

pub async fn serve(pool: PgPool) -> anyhow::Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&pool, msg).await,
            Err(e) => Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}})),
        };
        // Notifications (no id) get no reply.
        if let Some(reply) = reply {
            out.write_all(format!("{reply}\n").as_bytes()).await?;
            out.flush().await?;
        }
    }
    Ok(())
}

async fn handle(pool: &PgPool, msg: Value) -> Option<Value> {
    let id = msg.get("id")?.clone();
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    let result = match msg["method"].as_str().unwrap_or("") {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "job-radar", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Job Radar is João's personal search for remote software and AI jobs he can do from Brazil. Use search_jobs to find roles, get_job for details and prepared application drafts, update_job to track his pipeline, pipeline for an overview."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => Ok(call(pool, params["name"].as_str().unwrap_or(""), &params["arguments"]).await),
        other => Err(json!({"code": -32601, "message": format!("method not found: {other}")})),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
    })
}

/// Tool errors go back as results with isError, so the model can read them and adjust.
async fn call(pool: &PgPool, name: &str, args: &Value) -> Value {
    let out = match name {
        "search_jobs" => search(pool, args).await,
        "get_job" => get_job(pool, args).await,
        "update_job" => update(pool, args).await,
        "pipeline" => pipeline(pool).await,
        _ => Err(anyhow::anyhow!("unknown tool {name}")),
    };
    match out {
        Ok(v) => json!({
            "content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}],
            "structuredContent": v
        }),
        Err(e) => json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}),
    }
}

fn arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

async fn search(pool: &PgPool, args: &Value) -> anyhow::Result<Value> {
    let limit = args.get("limit").and_then(Value::as_i64).unwrap_or(15).clamp(1, 50);
    let filter = db::JobFilter {
        status: arg(args, "status").map(String::from),
        eligibility: arg(args, "eligibility").map(String::from),
        q: arg(args, "query").map(String::from),
        limit: Some(limit),
    };
    let jobs = db::list(pool, &filter).await?;
    let rows: Vec<Value> = jobs
        .iter()
        .map(|j| {
            json!({
                "id": j.id, "title": j.title, "company": j.company, "location": j.location,
                "eligibility": j.eligibility, "decided_by": j.checked_by, "score": j.score,
                "matched": j.matched, "salary": j.salary, "status": j.status, "url": j.url,
                "posted_at": j.posted_at.or(Some(j.first_seen))
            })
        })
        .collect();
    Ok(json!({"count": rows.len(), "jobs": rows}))
}

async fn get_job(pool: &PgPool, args: &Value) -> anyhow::Result<Value> {
    let id = arg(args, "id").ok_or_else(|| anyhow::anyhow!("id is required"))?;
    let jobs = db::list(pool, &db::JobFilter { status: Some("all".into()), eligibility: Some("all".into()), q: None, limit: Some(1000) }).await?;
    let job = jobs
        .into_iter()
        .find(|j| j.id == id)
        .ok_or_else(|| anyhow::anyhow!("no job with id {id}; use search_jobs to find ids"))?;
    let application = prep::get(pool, id).await?;
    Ok(json!({"job": job, "application": application}))
}

async fn update(pool: &PgPool, args: &Value) -> anyhow::Result<Value> {
    let id = arg(args, "id").ok_or_else(|| anyhow::anyhow!("id is required"))?;
    let status = arg(args, "status");
    if let Some(s) = status {
        anyhow::ensure!(db::STATUSES.contains(&s), "unknown status {s}");
    }
    let notes = args.get("notes").and_then(Value::as_str);
    anyhow::ensure!(status.is_some() || notes.is_some(), "give a status, notes, or both");
    let n = db::update(pool, id, status, notes).await?;
    anyhow::ensure!(n > 0, "no job with id {id}");
    Ok(json!({"updated": id, "status": status, "notes_changed": notes.is_some()}))
}

async fn pipeline(pool: &PgPool) -> anyhow::Result<Value> {
    let counts = db::status_counts(pool).await?;
    let new_today = db::new_today(pool).await?;
    let follow_up: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, title, company FROM jobs
         WHERE status = 'applied' AND status_changed_at < now() - interval '6 days'
         ORDER BY status_changed_at",
    )
    .fetch_all(pool)
    .await?;
    Ok(json!({
        "by_status": counts,
        "new_last_24h": new_today,
        "follow_up_due": follow_up.iter().map(|(id, t, c)| json!({"id": id, "title": t, "company": c})).collect::<Vec<_>>()
    }))
}
