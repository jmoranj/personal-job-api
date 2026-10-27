use std::collections::BTreeMap;

use anyhow::Context;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub refresh_hours: u64,
    pub profile: Profile,
    #[serde(default)]
    pub companies: Companies,
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
