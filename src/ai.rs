//! Calls Claude. Used for the "maybe" eligibility check and the application-prep agent.
//!
//! Two backends:
//! - `api`: the Claude API with ANTHROPIC_API_KEY (billed per token).
//! - `claude-code`: runs `claude -p` and uses the Claude Code subscription (no key).
//! `claude-code` is the default; set LLM_BACKEND=api to use the key.

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

/// One Claude request: instructions, the input, the JSON shape of the answer, and whether
/// Claude may search and read the web while answering.
pub struct Ask<'a> {
    pub system: &'a str,
    pub prompt: &'a str,
    pub schema: serde_json::Value,
    pub effort: &'a str,
    pub web: bool,
}

pub struct Classifier {
    backend: Backend,
}

fn eligibility_schema() -> serde_json::Value {
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
            ("api", Some(key)) => Backend::Api { client, key },
            ("claude-code", _) | ("", _) => Backend::ClaudeCode,
            (other, _) => bail!("unknown LLM_BACKEND {other:?}; use api or claude-code"),
        };
        Ok(Self { backend })
    }

    /// Checks the backend can run at all (the `claude` binary exists, or a key is set).
    pub async fn ready(&self) -> anyhow::Result<()> {
        if let Backend::ClaudeCode = self.backend {
            let out = tokio::process::Command::new(claude_bin())
                .arg("--version")
                .output()
                .await
                .context("`claude` not found; install Claude Code or set LLM_BACKEND=api")?;
            if !out.status.success() {
                bail!("`claude --version` failed");
            }
        }
        Ok(())
    }

    pub fn name(&self) -> &'static str {
        match self.backend {
            Backend::Api { .. } => "Claude API",
            Backend::ClaudeCode => "Claude Code (claude -p)",
        }
    }

    pub async fn classify(&self, title: &str, company: &str, location: &str, description: &str) -> anyhow::Result<(Verdict, Usage)> {
        let text = prompt(title, company, location, description);
        let (v, usage) = self
            .ask(&Ask { system: SYSTEM, prompt: &text, schema: eligibility_schema(), effort: "low", web: false })
            .await?;
        Ok((serde_json::from_value(v).context("parsing verdict")?, usage))
    }

    /// Returns Claude's answer as JSON matching `ask.schema`.
    pub async fn ask(&self, ask: &Ask<'_>) -> anyhow::Result<(serde_json::Value, Usage)> {
        match &self.backend {
            Backend::Api { client, key } => api(client, key, ask).await,
            Backend::ClaudeCode => claude_code(ask).await,
        }
    }
}

fn claude_bin() -> String {
    std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".into())
}

/// Runs Claude Code headless with our system prompt and JSON-schema output.
/// Only the web tools are available, and only when `ask.web` is set.
async fn claude_code(ask: &Ask<'_>) -> anyhow::Result<(serde_json::Value, Usage)> {
    use tokio::io::AsyncWriteExt;
    let tools = if ask.web { "WebSearch,WebFetch" } else { "" };
    let mut cmd = tokio::process::Command::new(claude_bin());
    cmd.args(["-p", "--model", MODEL, "--no-session-persistence", "--output-format", "json", "--tools", tools])
        .args(["--system-prompt", ask.system, "--json-schema", &ask.schema.to_string()]);
    if ask.web {
        cmd.args(["--allowedTools", "WebSearch", "WebFetch"]);
    }
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running `claude`; is Claude Code installed and logged in?")?;
    child.stdin.take().context("no stdin")?.write_all(ask.prompt.as_bytes()).await?;
    let out = child.wait_with_output().await?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .with_context(|| format!("claude -p failed: {}", String::from_utf8_lossy(&out.stderr).trim()))?;
    if v["is_error"].as_bool().unwrap_or(true) {
        bail!("claude -p: {}", v["result"]);
    }
    if v["structured_output"].is_null() {
        bail!("claude -p returned no structured output: {}", v["result"]);
    }
    let u = &v["usage"];
    let usage = Usage {
        input: u["input_tokens"].as_u64().unwrap_or(0) + u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
        output: u["output_tokens"].as_u64().unwrap_or(0),
        cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
        dollars: v["total_cost_usd"].as_f64().unwrap_or(0.0),
    };
    Ok((v["structured_output"].clone(), usage))
}

/// Calls the Messages API. With `ask.web`, Claude gets the server-side web search and fetch
/// tools; a long tool run can pause (`pause_turn`), so we resend until it finishes.
async fn api(client: &reqwest::Client, key: &str, ask: &Ask<'_>) -> anyhow::Result<(serde_json::Value, Usage)> {
    let mut messages = vec![json!({"role": "user", "content": ask.prompt})];
    let mut usage = Usage::default();
    for _turn in 0..6 {
        let mut body = json!({
            "model": MODEL,
            "max_tokens": 16000,
            "fallbacks": "default",
            "output_config": {
                "effort": ask.effort,
                "format": {"type": "json_schema", "schema": ask.schema}
            },
            "system": [{"type": "text", "text": ask.system, "cache_control": {"type": "ephemeral"}}],
            "messages": messages,
        });
        if ask.web {
            body["tools"] = json!([
                {"type": "web_search_20260209", "name": "web_search", "max_uses": 5},
                {"type": "web_fetch_20260209", "name": "web_fetch", "max_uses": 5}
            ]);
        }
        let v = send(client, key, &body).await?;
        let u = &v["usage"];
        let mut part = Usage {
            input: u["input_tokens"].as_u64().unwrap_or(0) + u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
            output: u["output_tokens"].as_u64().unwrap_or(0),
            cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
            dollars: 0.0,
        };
        // Claude Opus 5.5 list price: $4 / $20 per million tokens, cache reads $0.20.
        part.dollars = (part.input as f64 * 4.0 + part.output as f64 * 20.0 + part.cache_read as f64 * 0.2) / 1e6;
        usage.add(part);

        match v["stop_reason"].as_str() {
            Some("refusal") => bail!("Claude declined this request"),
            Some("pause_turn") => {
                messages.push(json!({"role": "assistant", "content": v["content"]}));
                continue;
            }
            _ => {}
        }
        let text = v["content"]
            .as_array()
            .and_then(|c| c.iter().rev().find(|b| b["type"] == "text"))
            .and_then(|b| b["text"].as_str())
            .context("no text in response")?;
        return Ok((serde_json::from_str(text).context("parsing Claude's JSON")?, usage));
    }
    bail!("Claude didn't finish after several tool rounds")
}

async fn send(client: &reqwest::Client, key: &str, body: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let mut wait = std::time::Duration::from_secs(2);
    for attempt in 0..4 {
        let resp = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", key)
            .header("anthropic-version", "2026-10-21")
            .header("anthropic-beta", "server-side-fallback-2026-07-01")
            .timeout(std::time::Duration::from_secs(600))
            .json(body)
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
        return Ok(v);
    }
    bail!("claude kept rate limiting")
}
