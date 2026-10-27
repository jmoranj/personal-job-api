//! Eligibility eval: compares the keyword rules and Claude against hand labels.
//!
//!   cargo run -- eval sample [n]   pick n jobs (default 150) into evals/eligibility.jsonl
//!   cargo run -- eval label        label them in the terminal (y / m / n, s to skip, q to quit)
//!   cargo run -- eval run          score rules and Claude on the labeled jobs

use std::collections::BTreeMap;
use std::io::{BufRead, Write};

use anyhow::{bail, Context};
use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::ai::{self, Classifier, Usage};

const SET: &str = "evals/eligibility.jsonl";
const LABELS: [&str; 3] = ["yes", "maybe", "no"];

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
struct Case {
    id: String,
    title: String,
    company: String,
    location: String,
    description: String,
    /// What the keyword rules said.
    rules: String,
    rules_reason: String,
    /// Hand label; None until labeled.
    #[sqlx(default)]
    label: Option<String>,
}

pub async fn main(args: &[String], pool: PgPool, client: reqwest::Client) -> anyhow::Result<()> {
    std::fs::create_dir_all("evals")?;
    match args.first().map(String::as_str) {
        Some("sample") => sample(&pool, args.get(1).and_then(|n| n.parse().ok()).unwrap_or(150)).await,
        Some("label") => label(),
        Some("run") => run(client).await,
        _ => bail!("usage: eval sample [n] | eval label | eval run"),
    }
}

fn load() -> anyhow::Result<Vec<Case>> {
    let raw = std::fs::read_to_string(SET).with_context(|| format!("reading {SET}; run `eval sample` first"))?;
    raw.lines().filter(|l| !l.trim().is_empty()).map(|l| Ok(serde_json::from_str(l)?)).collect()
}

fn save(cases: &[Case]) -> anyhow::Result<()> {
    let mut out = String::new();
    for c in cases {
        out.push_str(&serde_json::to_string(c)?);
        out.push('\n');
    }
    std::fs::write(SET, out)?;
    Ok(())
}

/// Equal thirds of what the rules called yes, maybe and no, so each class is tested.
async fn sample(pool: &PgPool, n: i64) -> anyhow::Result<()> {
    if std::path::Path::new(SET).exists() {
        bail!("{SET} exists; delete it to sample again (labels would be lost)");
    }
    let mut cases = Vec::new();
    for e in LABELS {
        let mut part: Vec<Case> = sqlx::query_as(
            "SELECT id, title, company, location, left(description, 6000) AS description,
                    eligibility AS rules, eligibility_reason AS rules_reason
             FROM jobs WHERE eligibility = $1 AND description <> '' ORDER BY random() LIMIT $2",
        )
        .bind(e)
        .bind(n / 3)
        .fetch_all(pool)
        .await?;
        cases.append(&mut part);
    }
    save(&cases)?;
    println!("wrote {} jobs to {SET}; next: cargo run -- eval label", cases.len());
    Ok(())
}

fn label() -> anyhow::Result<()> {
    let mut cases = load()?;
    let stdin = std::io::stdin();
    let todo = cases.iter().filter(|c| c.label.is_none()).count();
    let mut done = 0;
    for i in 0..cases.len() {
        if cases[i].label.is_some() {
            continue;
        }
        let c = &cases[i];
        println!("\n\x1b[1m[{}/{todo}] {} — {}\x1b[0m", done + 1, c.title, c.company);
        println!("Location: {}", if c.location.is_empty() { "(none)" } else { &c.location });
        println!("{}", hints(&c.description));
        print!("Can you do it from Brazil?  y / m(aybe) / n  ·  s skip  ·  d full description  ·  q quit > ");
        std::io::stdout().flush()?;
        loop {
            let mut line = String::new();
            stdin.lock().read_line(&mut line)?;
            let pick = match line.trim() {
                "y" => Some("yes"),
                "m" => Some("maybe"),
                "n" => Some("no"),
                "s" => break,
                "d" => {
                    println!("{}\n> ", c.description);
                    continue;
                }
                "q" => {
                    save(&cases)?;
                    println!("saved");
                    return Ok(());
                }
                _ => {
                    print!("y / m / n / s / d / q > ");
                    std::io::stdout().flush()?;
                    continue;
                }
            };
            cases[i].label = pick.map(String::from);
            done += 1;
            save(&cases)?;
            break;
        }
    }
    println!("all labeled; next: cargo run -- eval run");
    Ok(())
}

/// Sentences that mention where the job can be done, so labeling doesn't need the full text.
fn hints(description: &str) -> String {
    const WORDS: &[&str] = &[
        "remote", "location", "based in", "time zone", "timezone", "utc", "authoriz", "eligible", "country",
        "countries", "relocat", "visa", "on-site", "onsite", "hybrid", "anywhere", "worldwide", "latam", "brazil",
        "americas", "europe", "united states", "us-based", "contractor",
    ];
    let hits: Vec<&str> = description
        .split(['.', '\n'])
        .map(str::trim)
        .filter(|s| {
            let l = s.to_lowercase();
            WORDS.iter().any(|w| l.contains(w))
        })
        .take(8)
        .collect();
    if hits.is_empty() {
        "(no location sentences found — press d to read it)".into()
    } else {
        hits.iter().map(|s| format!("  · {s}")).collect::<Vec<_>>().join("\n")
    }
}

#[derive(Serialize, Deserialize)]
struct Prediction {
    id: String,
    eligibility: String,
    reason: String,
}

async fn run(client: reqwest::Client) -> anyhow::Result<()> {
    let cases: Vec<Case> = load()?.into_iter().filter(|c| c.label.is_some()).collect();
    if cases.is_empty() {
        bail!("no labeled jobs; run `eval label` first");
    }
    let claude = Classifier::from_env(client).context("set ANTHROPIC_API_KEY in .env to run the Claude side")?;

    // Predictions are cached per model so re-running the report costs nothing.
    let cache_path = format!("evals/predictions-{}.jsonl", ai::MODEL);
    let mut cache: BTreeMap<String, Prediction> = std::fs::read_to_string(&cache_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Prediction>(l).ok())
        .map(|p| (p.id.clone(), p))
        .collect();

    let missing: Vec<&Case> = cases.iter().filter(|c| !cache.contains_key(&c.id)).collect();
    println!("{} labeled jobs, {} need a Claude call", cases.len(), missing.len());
    let claude = &claude;
    let results: Vec<_> = stream::iter(missing)
        .map(|c| async move { (c.id.clone(), claude.classify(&c.title, &c.company, &c.location, &c.description).await) })
        .buffer_unordered(4)
        .collect()
        .await;

    let mut usage = Usage::default();
    let mut failed = 0;
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&cache_path)?;
    for (id, r) in results {
        match r {
            Ok((v, u)) => {
                usage.add(u);
                let p = Prediction { id: id.clone(), eligibility: v.eligibility, reason: v.reason };
                writeln!(file, "{}", serde_json::to_string(&p)?)?;
                cache.insert(id, p);
            }
            Err(e) => {
                failed += 1;
                eprintln!("{id}: {e:#}");
            }
        }
    }

    let gold: Vec<(&Case, &str)> = cases.iter().map(|c| (c, c.label.as_deref().unwrap())).collect();
    let rules: Vec<(&str, &str)> = gold.iter().map(|(c, g)| (*g, c.rules.as_str())).collect();
    let llm: Vec<(&str, &str)> =
        gold.iter().filter_map(|(c, g)| cache.get(&c.id).map(|p| (*g, p.eligibility.as_str()))).collect();
    // The setup we'd ship: rules decide yes/no, Claude decides only what the rules call maybe.
    let hybrid: Vec<(&str, &str)> = gold
        .iter()
        .filter_map(|(c, g)| {
            if c.rules != "maybe" {
                Some((*g, c.rules.as_str()))
            } else {
                cache.get(&c.id).map(|p| (*g, p.eligibility.as_str()))
            }
        })
        .collect();

    let mut report = String::new();
    report.push_str(&format!("# Eligibility eval — {}\n\n", chrono::Utc::now().format("%Y-%m-%d")));
    report.push_str(&format!("{} hand-labeled jobs. Model: `{}` (effort low).\n\n", cases.len(), ai::MODEL));
    report.push_str("| Classifier | Accuracy | \"Yes\" precision | \"Yes\" recall | Wrongly says \"no\" | Undecided (maybe) |\n|---|---|---|---|---|---|\n");
    for (name, pairs) in [("Keyword rules", &rules), ("Claude", &llm), ("Rules + Claude on maybe", &hybrid)] {
        report.push_str(&row(name, pairs));
    }
    report.push_str("\n**Wrongly says \"no\"** = jobs actually open to Brazil that would be hidden. It's the costliest mistake.\n\n");
    report.push_str("## Confusion (rows = label, columns = prediction)\n\n");
    for (name, pairs) in [("Keyword rules", &rules), ("Claude", &llm)] {
        report.push_str(&confusion(name, pairs));
    }
    report.push_str("## Claude disagreements\n\n");
    for (c, g) in &gold {
        if let Some(p) = cache.get(&c.id) {
            if p.eligibility != *g {
                report.push_str(&format!("- **{}** ({}): label {g}, Claude {} — {}\n", c.title, c.company, p.eligibility, p.reason));
            }
        }
    }
    if usage.input + usage.output > 0 {
        report.push_str(&format!(
            "\n## Cost of this run\n\n{} new calls, ${:.3} total, ${:.4} per job ({} input, {} cached, {} output tokens).\n",
            cases.len().saturating_sub(failed),
            usage.dollars(),
            usage.dollars() / (cases.len().saturating_sub(failed)).max(1) as f64,
            usage.input,
            usage.cache_read,
            usage.output
        ));
    }
    if failed > 0 {
        report.push_str(&format!("\n{failed} calls failed; run again to retry them.\n"));
    }
    std::fs::write("evals/REPORT.md", &report)?;
    println!("{report}\nwritten to evals/REPORT.md");
    Ok(())
}

fn row(name: &str, pairs: &[(&str, &str)]) -> String {
    let n = pairs.len().max(1) as f64;
    let correct = pairs.iter().filter(|(g, p)| g == p).count() as f64;
    let said_yes = pairs.iter().filter(|(_, p)| *p == "yes").count() as f64;
    let true_yes = pairs.iter().filter(|(g, p)| *g == "yes" && *p == "yes").count() as f64;
    let gold_yes = pairs.iter().filter(|(g, _)| *g == "yes").count() as f64;
    let hidden = pairs.iter().filter(|(g, p)| *g == "yes" && *p == "no").count();
    let maybe = pairs.iter().filter(|(_, p)| *p == "maybe").count() as f64;
    let pct = |x: f64, d: f64| if d == 0.0 { "—".to_string() } else { format!("{:.0}%", 100.0 * x / d) };
    format!(
        "| {name} | {} | {} | {} | {hidden} | {} |\n",
        pct(correct, n),
        pct(true_yes, said_yes),
        pct(true_yes, gold_yes),
        pct(maybe, n)
    )
}

fn confusion(name: &str, pairs: &[(&str, &str)]) -> String {
    let mut s = format!("**{name}**\n\n| | yes | maybe | no |\n|---|---|---|---|\n");
    for g in LABELS {
        s.push_str(&format!("| {g} |"));
        for p in LABELS {
            s.push_str(&format!(" {} |", pairs.iter().filter(|(a, b)| *a == g && *b == p).count()));
        }
        s.push('\n');
    }
    s.push('\n');
    s
}
