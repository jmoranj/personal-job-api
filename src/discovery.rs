//! Finds company career boards to watch.
//!
//! Three inputs:
//! 1. ATS links already present in fetched jobs (apply URLs, descriptions).
//! 2. Company directories: remoteintech (companies hiring worldwide or in the
//!    Americas) and Y Combinator companies that are hiring remotely.
//! 3. For directory names without a known board, slug guesses probed against
//!    Greenhouse, Lever, Ashby and Workable. A guess counts only if the board
//!    has at least one open job, which also rules out unrelated empty boards.

use std::collections::BTreeSet;
use std::io::Read;
use std::sync::LazyLock;

use futures::StreamExt;
use regex::Regex;
use reqwest::Client;
use serde::Serialize;
use sqlx::PgPool;

use crate::infra::db;
use crate::sources::{fetch_board, Board, PROBED_ATS};

const REMOTEINTECH_TARBALL: &str = "https://codeload.github.com/remoteintech/remote-jobs/tar.gz/refs/heads/main";
const YC_HIRING: &str = "https://yc-oss.github.io/api/companies/hiring.json";
/// remoteintech regions that can include someone in Brazil.
const OPEN_REGIONS: &[&str] = &["worldwide", "americas", "americas-europe"];
const PROBE_CONCURRENCY: usize = 12;
/// Upper bound on candidates probed in one run, so a run stays a few minutes long.
const PROBE_BATCH: i64 = 1500;

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub from_links: usize,
    pub candidates_added: usize,
    pub probed: usize,
    pub found: usize,
}

static ATS_LINKS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        ("greenhouse", r"(?:boards|job-boards)\.greenhouse\.io/(?:embed/job_board\?for=)?([a-z0-9_-]+)"),
        ("lever", r"jobs\.lever\.co/([a-z0-9_.-]+)"),
        ("ashby", r"jobs\.ashbyhq\.com/([A-Za-z0-9_.-]+)"),
        ("workable", r"apply\.workable\.com/([a-z0-9_-]+)"),
        ("recruitee", r"([a-z0-9-]+)\.recruitee\.com"),
        ("teamtailor", r"([a-z0-9-]+)\.teamtailor\.com"),
        ("personio", r"([a-z0-9-]+)\.jobs\.personio\.(?:de|com)"),
    ]
    .into_iter()
    .map(|(ats, re)| (ats, Regex::new(&format!("(?i){re}")).unwrap()))
    .collect()
});

/// Path segments and subdomains that look like slugs but aren't companies.
const NOT_SLUGS: &[&str] = &["j", "api", "embed", "www", "app", "support", "help", "careers", "jobs", "v1", "static"];

/// Every (ats, slug) pair mentioned in a piece of text.
pub fn board_links(text: &str) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    for (ats, re) in ATS_LINKS.iter() {
        for cap in re.captures_iter(text) {
            let slug = cap[1].trim_end_matches(['.', '-']).to_string();
            if slug.len() >= 2 && !NOT_SLUGS.contains(&slug.to_lowercase().as_str()) {
                // Ashby slugs keep their case; the others are case-insensitive.
                let slug = if *ats == "ashby" { slug } else { slug.to_lowercase() };
                out.insert((ats.to_string(), slug));
            }
        }
    }
    out
}

/// Likely board slugs for a company: its directory slug, its name squashed, and its name dashed.
pub fn slug_guesses(name: &str, hint: &str) -> Vec<String> {
    let clean = name
        .to_lowercase()
        .replace(['.', ','], " ")
        .split_whitespace()
        .filter(|w| !["inc", "llc", "ltd", "gmbh", "co", "corp", "the"].contains(w))
        .collect::<Vec<_>>()
        .join(" ");
    let words: Vec<String> = clean.split_whitespace().map(|w| w.chars().filter(|c| c.is_ascii_alphanumeric()).collect()).filter(|w: &String| !w.is_empty()).collect();
    let mut out = Vec::new();
    for g in [hint.to_lowercase(), words.concat(), words.join("-")] {
        if g.len() >= 2 && !out.contains(&g) {
            out.push(g);
        }
    }
    out
}

pub async fn run(pool: &PgPool, client: &Client) -> anyhow::Result<Report> {
    let mut report = Report::default();

    // 1. Links in jobs already stored.
    for (ats, slug) in board_links(&db::job_link_text(pool).await?) {
        if db::add_company(pool, &ats, &slug, "", "job link").await? {
            report.from_links += 1;
        }
    }

    // 2. Directories.
    match remoteintech(client).await {
        Ok(entries) => {
            for e in entries {
                let direct = board_links(&e.careers_url);
                if direct.is_empty() {
                    report.candidates_added += db::add_candidate(pool, &e.name, &e.slug, "remoteintech").await? as usize;
                } else {
                    for (ats, slug) in direct {
                        report.from_links += db::add_company(pool, &ats, &slug, &e.name, "remoteintech").await? as usize;
                    }
                }
            }
        }
        Err(e) => tracing::warn!("remoteintech directory failed: {e:#}"),
    }
    match yc_remote(client).await {
        Ok(entries) => {
            for (name, slug) in entries {
                report.candidates_added += db::add_candidate(pool, &name, &slug, "y combinator").await? as usize;
            }
        }
        Err(e) => tracing::warn!("YC directory failed: {e:#}"),
    }

    // 3. Probe candidates that haven't been tried (or were missed over 30 days ago).
    let candidates = db::candidates_to_probe(pool, PROBE_BATCH).await?;
    report.probed = candidates.len();
    let hits: Vec<(String, Vec<Board>)> = futures::stream::iter(candidates)
        .map(|c| async move {
            let mut found = Vec::new();
            for ats in PROBED_ATS {
                for slug in slug_guesses(&c.name, &c.slug_hint) {
                    let b = Board { ats: ats.to_string(), slug, name: c.name.clone() };
                    if matches!(fetch_board(client, &b).await, Ok(jobs) if !jobs.is_empty()) {
                        found.push(b);
                        break;
                    }
                }
            }
            (c.key, found)
        })
        .buffer_unordered(PROBE_CONCURRENCY)
        .collect()
        .await;
    for (key, boards) in hits {
        for b in &boards {
            report.found += db::add_company(pool, &b.ats, &b.slug, &b.name, "probe").await? as usize;
        }
        db::mark_probed(pool, &key, !boards.is_empty()).await?;
    }

    Ok(report)
}

pub struct DirectoryEntry {
    pub name: String,
    pub slug: String,
    pub careers_url: String,
}

static FRONTMATTER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)^---\n(.*?)\n---").unwrap());

fn front(fm: &str, key: &str) -> String {
    Regex::new(&format!(r#"(?m)^{key}:\s*"?(.*?)"?\s*$"#))
        .unwrap()
        .captures(fm)
        .map(|c| c[1].to_string())
        .unwrap_or_default()
}

/// remoteintech company profiles whose region can include Brazil.
async fn remoteintech(c: &Client) -> anyhow::Result<Vec<DirectoryEntry>> {
    let bytes = c.get(REMOTEINTECH_TARBALL).send().await?.error_for_status()?.bytes().await?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]));
    let mut out = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        if !path.contains("/src/companies/") || !path.ends_with(".md") {
            continue;
        }
        let mut text = String::new();
        entry.read_to_string(&mut text)?;
        let Some(fm) = FRONTMATTER.captures(&text) else { continue };
        let fm = &fm[1];
        if !OPEN_REGIONS.contains(&front(fm, "region").as_str()) {
            continue;
        }
        out.push(DirectoryEntry { name: front(fm, "title"), slug: front(fm, "slug"), careers_url: front(fm, "careers_url") });
    }
    Ok(out)
}

/// Y Combinator companies that are hiring and list Remote among their regions.
async fn yc_remote(c: &Client) -> anyhow::Result<Vec<(String, String)>> {
    let v: serde_json::Value = c.get(YC_HIRING).send().await?.error_for_status()?.json().await?;
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .filter(|co| co["regions"].as_array().is_some_and(|r| r.iter().any(|x| x.as_str() == Some("Remote"))))
        .filter_map(|co| Some((co["name"].as_str()?.to_string(), co["slug"].as_str().unwrap_or_default().to_string())))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_board_links() {
        let text = "Apply at https://jobs.ashbyhq.com/PostHog/123 or https://boards.greenhouse.io/gitlab/jobs/1 \
                    and https://apply.workable.com/j/ABC plus https://acme.recruitee.com/o/dev";
        let found = board_links(text);
        assert!(found.contains(&("ashby".into(), "PostHog".into())));
        assert!(found.contains(&("greenhouse".into(), "gitlab".into())));
        assert!(found.contains(&("recruitee".into(), "acme".into())));
        assert!(!found.iter().any(|(a, s)| a == "workable" && s == "j"));
    }

    #[test]
    fn guesses_slugs() {
        assert_eq!(slug_guesses("Hugging Face, Inc.", ""), vec!["huggingface", "hugging-face"]);
        assert_eq!(slug_guesses("GitLab", "gitlab"), vec!["gitlab"]);
    }
}
