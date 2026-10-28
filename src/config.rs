use std::collections::BTreeMap;

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub refresh_hours: u64,
    pub profile: Profile,
    #[serde(default)]
    pub companies: Companies,
    #[serde(default)]
    pub ai: Ai,
}

/// Claude checks the jobs the keyword rules leave as "maybe".
#[derive(Debug, Clone, Deserialize)]
pub struct Ai {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Jobs checked per search; the rest wait for the next one.
    #[serde(default = "per_run")]
    pub max_per_run: i64,
}

impl Default for Ai {
    fn default() -> Self {
        Self { enabled: true, max_per_run: per_run() }
    }
}

fn yes() -> bool {
    true
}

fn per_run() -> i64 {
    100
}

#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    pub title_keywords: Vec<String>,
    #[serde(default)]
    pub exclude_title: Vec<String>,
    #[serde(default)]
    pub seniority: Vec<String>,
    #[serde(default)]
    pub skills: BTreeMap<String, i32>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Companies {
    #[serde(default)]
    pub greenhouse: Vec<String>,
    #[serde(default)]
    pub lever: Vec<String>,
    #[serde(default)]
    pub ashby: Vec<String>,
}

impl Config {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
        toml::from_str(&raw).with_context(|| format!("parsing {path}"))
    }
}
