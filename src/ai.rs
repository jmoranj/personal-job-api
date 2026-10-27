//! Asks Claude whether a job can be done from Brazil, for jobs the rules can't decide.

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
}

impl Usage {
    /// Claude Opus 5.5 list price: $4 / $20 per million tokens, cache reads $0.20.
    pub fn dollars(&self) -> f64 {
        (self.input as f64 * 4.0 + self.output as f64 * 20.0 + self.cache_read as f64 * 0.2) / 1e6
    }
    pub fn add(&mut self, o: Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
    }
}

pub struct Classifier {
    client: reqwest::Client,
    key: String,
}

impl Classifier {
    /// Returns None when ANTHROPIC_API_KEY is not set.
    pub fn from_env(client: reqwest::Client) -> Option<Self> {
        let key = std::env::var("ANTHROPIC_API_KEY").ok().filter(|k| !k.is_empty())?;
        Some(Self { client, key })
    }

    pub async fn classify(&self, title: &str, company: &str, location: &str, description: &str) -> anyhow::Result<(Verdict, Usage)> {
        let description: String = description.chars().take(6000).collect();
        let body = json!({
            "model": MODEL,
            "max_tokens": 2000,
            "fallbacks": "default",
            "output_config": {
                "effort": "low",
                "format": {
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "eligibility": {"type": "string", "enum": ["yes", "maybe", "no"]},
                            "reason": {"type": "string"}
                        },
                        "required": ["eligibility", "reason"],
                        "additionalProperties": false
                    }
                }
            },
            "system": [{"type": "text", "text": SYSTEM, "cache_control": {"type": "ephemeral"}}],
            "messages": [{
                "role": "user",
                "content": format!("Title: {title}\nCompany: {company}\nLocation: {location}\n\nDescription:\n{description}")
            }]
        });

        let mut wait = std::time::Duration::from_secs(2);
        for attempt in 0..4 {
            let resp = self
                .client
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.key)
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
            let usage = Usage {
                input: u["input_tokens"].as_u64().unwrap_or(0) + u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
                output: u["output_tokens"].as_u64().unwrap_or(0),
                cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
            };
            return Ok((verdict, usage));
        }
        bail!("claude kept rate limiting")
    }
}
