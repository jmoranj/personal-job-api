//! Asks Claude whether a job can be done from Brazil, for jobs the rules can't decide.
//!
//! Two backends:
//! - `api`: the Claude API with ANTHROPIC_API_KEY (billed per token).
//! - `claude-code`: runs `claude -p` and uses the Claude Code subscription (no key).
//! Picked by LLM_BACKEND, else `api` when a key is set, else `claude-code`.

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const MODEL: &str = "claude-opus-5-5";

const SYSTEM: &str = "You decide whether a remote job posting can be done by a software engineer living in Brazil, \
working from Brazil (as a contractor or employee). Read the location and description.

Answer:
- \"yes\": the posting explicitly allows Brazil, LATAM, South America, the Americas, worldwide, or any country, \
or hires internationally through an employer of record / contractors with no country restriction that excludes Brazil.
- \"no\": it requires living or being authorized to work in a specific country or region that excludes Brazil \
(e.g. US only, EU only, UK, Canada, India), requires on-site or hybrid presence outside Brazil, or requires \
overlap that is impossible from UTC-3.
- \"maybe\": the posting says remote but gives no clear country rule either way.

Prefer \"maybe\" over guessing. The reason must quote or paraphrase the deciding phrase in under 25 words.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub eligibility: String,
    pub reason: String,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// List-price dollars. With `claude-code` this is what the API would have charged;
    /// the subscription is not billed per call.
    pub dollars: f64,
}

impl Usage {
    pub fn add(&mut self, o: Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.dollars += o.dollars;
    }
}

enum Backend {
    Api { client: reqwest::Client, key: String },
    ClaudeCode,
}

pub struct Classifier {
    backend: Backend,
}

fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "properties": {
            "eligibility": {"type": "string", "enum": ["yes", "maybe", "no"]},
            "reason": {"type": "string"}
        },
        "required": ["eligibility", "reason"],
        "additionalProperties": false
    })
}

fn prompt(title: &str, company: &str, location: &str, description: &str) -> String {
    let description: String = description.chars().take(6000).collect();
    format!("Title: {title}\nCompany: {company}\nLocation: {location}\n\nDescription:\n{description}")
}

impl Classifier {
    pub fn from_env(client: reqwest::Client) -> anyhow::Result<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty());
        let choice = std::env::var("LLM_BACKEND").unwrap_or_default();
        let backend = match (choice.as_str(), key) {
            ("api", None) => bail!("LLM_BACKEND=api but ANTHROPIC_API_KEY is not set"),
            ("api", Some(key)) | ("", Some(key)) => Backend::Api { client, key },
            ("claude-code", _) | ("", None) => Backend::ClaudeCode,
            (other, _) => bail!("unknown LLM_BACKEND {other:?}; use api or claude-code"),
        };
        Ok(Self { backend })
    }

    pub fn name(&self) -> &'static str {
        match self.backend {
            Backend::Api { .. } => "Claude API",
            Backend::ClaudeCode => "Claude Code (claude -p)",
        }
    }

    pub async fn classify(&self, title: &str, company: &str, location: &str, description: &str) -> anyhow::Result<(Verdict, Usage)> {
        let text = prompt(title, company, location, description);
        match &self.backend {
            Backend::Api { client, key } => api(client, key, &text).await,
            Backend::ClaudeCode => claude_code(&text).await,
        }
    }
}

/// Runs Claude Code headless: no tools, our system prompt, JSON-schema output.
async fn claude_code(text: &str) -> anyhow::Result<(Verdict, Usage)> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".into()))
        .args(["-p", "--model", MODEL, "--no-session-persistence", "--output-format", "json", "--tools", ""])
        .args(["--system-prompt", SYSTEM, "--json-schema", &schema().to_string()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running `claude`; is Claude Code installed and logged in?")?;
    child.stdin.take().context("no stdin")?.write_all(text.as_bytes()).await?;
    let out = child.wait_with_output().await?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .with_context(|| format!("claude -p failed: {}", String::from_utf8_lossy(&out.stderr).trim()))?;
    if v["is_error"].as_bool().unwrap_or(true) {
        bail!("claude -p: {}", v["result"]);
    }
    let verdict: Verdict = serde_json::from_value(v["structured_output"].clone()).context("parsing verdict")?;
    let u = &v["usage"];
    let usage = Usage {
        input: u["input_tokens"].as_u64().unwrap_or(0) + u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
        output: u["output_tokens"].as_u64().unwrap_or(0),
        cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
        dollars: v["total_cost_usd"].as_f64().unwrap_or(0.0),
    };
    Ok((verdict, usage))
}

async fn api(client: &reqwest::Client, key: &str, text: &str) -> anyhow::Result<(Verdict, Usage)> {
    {
        let body = json!({
            "model": MODEL,
            "max_tokens": 2000,
            "fallbacks": "default",
            "output_config": {
                "effort": "low",
                "format": {"type": "json_schema", "schema": schema()}
            },
            "system": [{"type": "text", "text": SYSTEM, "cache_control": {"type": "ephemeral"}}],
            "messages": [{
                "role": "user",
                "content": text
            }]
        });

        let mut wait = std::time::Duration::from_secs(2);
        for attempt in 0..4 {
            let resp = client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", key)
                .header("anthropic-version", "2026-10-21")
                .header("anthropic-beta", "server-side-fallback-2026-07-01")
                .json(&body)
                .send()
                .await?;
            let status = resp.status();
            if (status.as_u16() == 429 || status.is_server_error()) && attempt < 3 {
                tokio::time::sleep(wait).await;
                wait *= 3;
                continue;
            }
            let v: serde_json::Value = resp.json().await?;
            if !status.is_success() {
                bail!("claude {status}: {}", v["error"]["message"]);
            }
            if v["stop_reason"] == "refusal" {
                bail!("claude declined this posting");
            }
            let text = v["content"]
                .as_array()
                .and_then(|c| c.iter().find(|b| b["type"] == "text"))
                .and_then(|b| b["text"].as_str())
                .context("no text in response")?;
            let verdict: Verdict = serde_json::from_str(text).context("parsing verdict")?;
            let u = &v["usage"];
            let mut usage = Usage {
                input: u["input_tokens"].as_u64().unwrap_or(0) + u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
                output: u["output_tokens"].as_u64().unwrap_or(0),
                cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                dollars: 0.0,
            };
            // Claude Opus 5.5 list price: $4 / $20 per million tokens, cache reads $0.20.
            usage.dollars = (usage.input as f64 * 4.0 + usage.output as f64 * 20.0 + usage.cache_read as f64 * 0.2) / 1e6;
            return Ok((verdict, usage));
        }
        bail!("claude kept rate limiting")
    }
}
