//! Fetchers for each job board. Each one returns jobs in a common shape.

use std::sync::LazyLock;

use chrono::{DateTime, TimeZone, Utc};
use regex::Regex;
use reqwest::Client;
use serde_json::Value;

use futures::{FutureExt, StreamExt};

#[derive(Debug, Clone)]
pub struct RawJob {
    pub source: String,
    pub title: String,
    pub company: String,
    pub url: String,
    pub location: String,
    pub tags: Vec<String>,
    pub salary: Option<String>,
    pub posted_at: Option<DateTime<Utc>>,
    pub description: String,
}

pub struct SourceResult {
    pub source: String,
    /// Set when the result came from one company's career board.
    pub board: Option<Board>,
    pub jobs: anyhow::Result<Vec<RawJob>>,
}

/// One company's career board on an applicant tracking system.
#[derive(Debug, Clone)]
pub struct Board {
    pub ats: String,
    pub slug: String,
    pub name: String,
}

/// ATS names as stored in the database, in the order discovery probes them.
pub const PROBED_ATS: &[&str] = &["greenhouse", "lever", "ashby", "workable"];

pub fn ats_label(ats: &str) -> &'static str {
    match ats {
        "greenhouse" => "Greenhouse",
        "lever" => "Lever",
        "ashby" => "Ashby",
        "workable" => "Workable",
        "recruitee" => "Recruitee",
        "teamtailor" => "Teamtailor",
        "personio" => "Personio",
        _ => "Company board",
    }
}

pub async fn fetch_board(c: &Client, b: &Board) -> anyhow::Result<Vec<RawJob>> {
    let name = if b.name.is_empty() { b.slug.clone() } else { b.name.clone() };
    let mut jobs = match b.ats.as_str() {
        "greenhouse" => greenhouse(c, b.slug.clone()).await?,
        "lever" => lever(c, b.slug.clone()).await?,
        "ashby" => ashby(c, b.slug.clone()).await?,
        "workable" => workable(c, &b.slug).await?,
        "recruitee" => recruitee(c, &b.slug).await?,
        "teamtailor" => teamtailor(c, &b.slug).await?,
        "personio" => personio(c, &b.slug).await?,
        other => anyhow::bail!("unsupported ATS {other}"),
    };
    // Lever and Ashby don't return the company name; use the one discovery recorded.
    for j in &mut jobs {
        if j.company.is_empty() || j.company == b.slug {
            j.company = name.clone();
        }
    }
    Ok(jobs)
}

const DESCRIPTION_LIMIT: usize = 6000;

static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").unwrap());
static SPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&#x2F;", "/")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// HTML (possibly entity-escaped) to plain text.
pub fn plain(html: &str) -> String {
    let decoded = unescape(html);
    let text = TAG_RE.replace_all(&decoded, " ");
    let text = SPACE_RE.replace_all(&unescape(&text), " ").trim().to_string();
    text.chars().take(DESCRIPTION_LIMIT).collect()
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string()
}

fn str_list(v: &Value, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn rfc3339(v: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(v)
        .map(|d| d.with_timezone(&Utc))
        .ok()
        .or_else(|| chrono::NaiveDateTime::parse_from_str(v, "%Y-%m-%dT%H:%M:%S").ok().map(|n| n.and_utc()))
}

fn epoch(v: Option<i64>) -> Option<DateTime<Utc>> {
    v.and_then(|t| Utc.timestamp_opt(t, 0).single())
}

fn money(min: Option<f64>, max: Option<f64>, currency: &str) -> Option<String> {
    let cur = if currency.is_empty() { "USD" } else { currency };
    let k = |n: f64| format!("{}k", (n / 1000.0).round());
    match (min.filter(|n| *n > 0.0), max.filter(|n| *n > 0.0)) {
        (Some(a), Some(b)) => Some(format!("{}–{} {cur}", k(a), k(b))),
        (Some(a), None) | (None, Some(a)) => Some(format!("{} {cur}", k(a))),
        _ => None,
    }
}

/// GET a JSON document, backing off and retrying when the host rate-limits (Workable does at ~16 parallel calls).
async fn json(client: &Client, url: &str) -> anyhow::Result<Value> {
    let mut wait = std::time::Duration::from_secs(2);
    for _ in 0..3 {
        let res = client.get(url).send().await?;
        if res.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Ok(res.error_for_status()?.json().await?);
        }
        tokio::time::sleep(wait).await;
        wait *= 3;
    }
    Ok(client.get(url).send().await?.error_for_status()?.json().await?)
}

/// How many company boards are fetched at the same time.
const BOARD_CONCURRENCY: usize = 16;

pub async fn fetch_all(client: &Client, boards: &[Board]) -> Vec<SourceResult> {
    let job_boards = futures::future::join_all(vec![
        run("Remotive", None, remotive(client)).boxed(),
        run("Remote OK", None, remoteok(client)).boxed(),
        run("Himalayas", None, himalayas(client)).boxed(),
        run("Jobicy", None, jobicy(client)).boxed(),
        run("We Work Remotely", None, wwr(client)).boxed(),
        run("Arbeitnow", None, arbeitnow(client)).boxed(),
        run("Hacker News", None, hackernews(client)).boxed(),
    ]);
    let company_boards = futures::stream::iter(boards.iter().cloned())
        .map(|b| async move {
            let name = format!("{}: {}", ats_label(&b.ats), if b.name.is_empty() { &b.slug } else { &b.name });
            run(&name, Some(b.clone()), fetch_board(client, &b)).await
        })
        .buffer_unordered(BOARD_CONCURRENCY)
        .collect::<Vec<_>>();
    let (mut out, companies) = futures::join!(job_boards, company_boards);
    out.extend(companies);
    out
}

async fn run(name: &str, board: Option<Board>, f: impl std::future::Future<Output = anyhow::Result<Vec<RawJob>>>) -> SourceResult {
    let jobs = f.await.map(|mut v| {
        for j in &mut v {
            j.source = name.to_string();
        }
        v
    });
    SourceResult { source: name.to_string(), board, jobs }
}

fn job(title: String, company: String, url: String, location: String) -> RawJob {
    RawJob {
        source: String::new(),
        title,
        company,
        url,
        location,
        tags: vec![],
        salary: None,
        posted_at: None,
        description: String::new(),
    }
}

async fn remotive(c: &Client) -> anyhow::Result<Vec<RawJob>> {
    let v = json(c, "https://remotive.com/api/remote-jobs").await?;
    Ok(v["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|j| RawJob {
            tags: str_list(j, "tags"),
            salary: Some(s(j, "salary")).filter(|x| !x.is_empty()),
            posted_at: rfc3339(&s(j, "publication_date")),
            description: plain(&s(j, "description")),
            ..job(s(j, "title"), s(j, "company_name"), s(j, "url"), s(j, "candidate_required_location"))
        })
        .collect())
}

async fn remoteok(c: &Client) -> anyhow::Result<Vec<RawJob>> {
    let v = json(c, "https://remoteok.com/api").await?;
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .filter(|j| j.get("position").is_some())
        .map(|j| RawJob {
            tags: str_list(j, "tags"),
            salary: money(j["salary_min"].as_f64(), j["salary_max"].as_f64(), "USD"),
            posted_at: epoch(j["epoch"].as_i64()),
            description: plain(&s(j, "description")),
            ..job(s(j, "position"), s(j, "company"), s(j, "url"), s(j, "location"))
        })
        .collect())
}

async fn himalayas(c: &Client) -> anyhow::Result<Vec<RawJob>> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let url = match &cursor {
            Some(cur) => format!("https://himalayas.app/jobs/api?limit=100&cursor={cur}"),
            None => "https://himalayas.app/jobs/api?limit=100".to_string(),
        };
        let v = json(c, &url).await?;
        for j in v["jobs"].as_array().into_iter().flatten() {
            let places = str_list(j, "locationRestrictions");
            let location = if places.is_empty() { "Worldwide".to_string() } else { places.join(", ") };
            out.push(RawJob {
                tags: str_list(j, "categories"),
                salary: money(j["minSalary"].as_f64(), j["maxSalary"].as_f64(), &s(j, "currency")),
                posted_at: epoch(j["pubDate"].as_i64()),
                description: plain(&s(j, "description")),
                ..job(s(j, "title"), s(j, "companyName"), s(j, "applicationLink"), location)
            });
        }
        cursor = v["nextCursor"].as_str().map(str::to_string);
        if cursor.is_none() {
            break;
        }
    }
    Ok(out)
}

async fn jobicy(c: &Client) -> anyhow::Result<Vec<RawJob>> {
    let v = json(c, "https://jobicy.com/api/v2/remote-jobs?count=100&industry=engineering").await?;
    Ok(v["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|j| RawJob {
            tags: str_list(j, "jobIndustry"),
            salary: money(j["salaryMin"].as_f64(), j["salaryMax"].as_f64(), &s(j, "salaryCurrency")),
            posted_at: rfc3339(&s(j, "pubDate")),
            description: plain(&s(j, "jobDescription")),
            ..job(s(j, "jobTitle"), s(j, "companyName"), s(j, "url"), s(j, "jobGeo"))
        })
        .collect())
}

static ITEM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<item>(.*?)</item>").unwrap());

fn xml_field(item: &str, tag: &str) -> String {
    let re = Regex::new(&format!(r"(?s)<{tag}>(.*?)</{tag}>")).unwrap();
    re.captures(item)
        .map(|c| c[1].trim().trim_start_matches("<![CDATA[").trim_end_matches("]]>").to_string())
        .unwrap_or_default()
}

async fn wwr(c: &Client) -> anyhow::Result<Vec<RawJob>> {
    let body = c
        .get("https://weworkremotely.com/categories/remote-programming-jobs.rss")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(ITEM_RE
        .captures_iter(&body)
        .map(|cap| {
            let item = &cap[1];
            let full = unescape(&xml_field(item, "title"));
            let (company, title) = full.split_once(": ").map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((String::new(), full.clone()));
            RawJob {
                tags: vec![xml_field(item, "category")],
                posted_at: DateTime::parse_from_rfc2822(&xml_field(item, "pubDate")).ok().map(|d| d.with_timezone(&Utc)),
                description: plain(&xml_field(item, "description")),
                ..job(title, company, xml_field(item, "link"), unescape(&xml_field(item, "region")))
            }
        })
        .collect())
}

async fn arbeitnow(c: &Client) -> anyhow::Result<Vec<RawJob>> {
    let mut out = Vec::new();
    for page in 1..=3 {
        let v = json(c, &format!("https://www.arbeitnow.com/api/job-board-api?page={page}")).await?;
        for j in v["data"].as_array().into_iter().flatten().filter(|j| j["remote"].as_bool() == Some(true)) {
            out.push(RawJob {
                tags: str_list(j, "tags"),
                posted_at: epoch(j["created_at"].as_i64()),
                description: plain(&s(j, "description")),
                ..job(s(j, "title"), s(j, "company_name"), s(j, "url"), s(j, "location"))
            });
        }
    }
    Ok(out)
}

/// Top-level comments of the latest "Ask HN: Who is hiring?" thread that mention remote work.
async fn hackernews(c: &Client) -> anyhow::Result<Vec<RawJob>> {
    let search = json(
        c,
        "https://hn.algolia.com/api/v1/search_by_date?tags=story,author_whoishiring&query=who%20is%20hiring&hitsPerPage=5",
    )
    .await?;
    let story = search["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|h| s(h, "title").to_lowercase().contains("who is hiring"))
        .ok_or_else(|| anyhow::anyhow!("no Who is hiring thread found"))?;
    let id = s(story, "objectID");
    let thread = json(c, &format!("https://hn.algolia.com/api/v1/items/{id}")).await?;
    let posted = rfc3339(&s(&thread, "created_at"));
    Ok(thread["children"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|cm| {
            let html = cm["text"].as_str()?;
            let text = plain(html);
            if !text.to_lowercase().contains("remote") {
                return None;
            }
            // First line is usually "Company | Role | Location | ..."
            let first = plain(html.split("<p>").next().unwrap_or(html));
            let parts: Vec<&str> = first.split('|').map(str::trim).collect();
            if parts.len() < 2 {
                return None;
            }
            let title = parts.iter().skip(1).find(|p| p.to_lowercase().contains("engineer") || p.to_lowercase().contains("developer")).unwrap_or(&parts[1]);
            let cid = cm["id"].as_i64().unwrap_or_default();
            Some(RawJob {
                posted_at: cm["created_at"].as_str().and_then(rfc3339).or(posted),
                description: text,
                ..job(
                    title.chars().take(140).collect(),
                    parts[0].chars().take(80).collect(),
                    format!("https://news.ycombinator.com/item?id={cid}"),
                    parts[1..].join(" | ").chars().take(200).collect(),
                )
            })
        })
        .collect())
}

async fn greenhouse(c: &Client, slug: String) -> anyhow::Result<Vec<RawJob>> {
    let v = json(c, &format!("https://boards-api.greenhouse.io/v1/boards/{slug}/jobs?content=true")).await?;
    let company = v["jobs"][0]["company_name"].as_str().map(str::to_string).unwrap_or_else(|| slug.clone());
    Ok(v["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|j| RawJob {
            posted_at: rfc3339(&s(j, "updated_at")),
            description: plain(&s(j, "content")),
            ..job(s(j, "title"), company.clone(), s(j, "absolute_url"), s(&j["location"], "name"))
        })
        .collect())
}

async fn lever(c: &Client, slug: String) -> anyhow::Result<Vec<RawJob>> {
    let v = json(c, &format!("https://api.lever.co/v0/postings/{slug}?mode=json")).await?;
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .map(|j| {
            let cat = &j["categories"];
            let mut places = str_list(cat, "allLocations");
            if places.is_empty() {
                places.push(s(cat, "location"));
            }
            RawJob {
                tags: [s(cat, "team"), s(cat, "commitment"), s(j, "workplaceType")].into_iter().filter(|x| !x.is_empty()).collect(),
                posted_at: j["createdAt"].as_i64().and_then(|ms| Utc.timestamp_millis_opt(ms).single()),
                description: plain(&format!("{} {}", s(j, "descriptionPlain"), s(j, "additionalPlain"))),
                ..job(s(j, "text"), slug.clone(), s(j, "hostedUrl"), places.join(", "))
            }
        })
        .collect())
}

async fn ashby(c: &Client, slug: String) -> anyhow::Result<Vec<RawJob>> {
    let v = json(c, &format!("https://api.ashbyhq.com/posting-api/job-board/{slug}?includeCompensation=true")).await?;
    Ok(v["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|j| {
            let mut places = vec![s(j, "location")];
            for sec in j["secondaryLocations"].as_array().into_iter().flatten() {
                places.push(s(sec, "location"));
            }
            places.retain(|p| !p.is_empty());
            RawJob {
                tags: [s(j, "department"), s(j, "employmentType"), s(j, "workplaceType")].into_iter().filter(|x| !x.is_empty()).collect(),
                salary: j["compensation"]["compensationTierSummary"].as_str().map(str::to_string),
                posted_at: rfc3339(&s(j, "publishedAt")),
                description: plain(&s(j, "descriptionPlain")),
                ..job(s(j, "title"), slug.clone(), s(j, "jobUrl"), places.join(", "))
            }
        })
        .collect())
}

async fn workable(c: &Client, slug: &str) -> anyhow::Result<Vec<RawJob>> {
    let v = json(c, &format!("https://apply.workable.com/api/v1/widget/accounts/{slug}")).await?;
    let company = s(&v, "name");
    Ok(v["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|j| {
            let mut places: Vec<String> = j["locations"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|l| [s(l, "city"), s(l, "country")].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(", "))
                .filter(|p| !p.is_empty())
                .collect();
            if j["telecommuting"].as_bool() == Some(true) {
                places.insert(0, "Remote".into());
            }
            RawJob {
                tags: [s(j, "department"), s(j, "function"), s(j, "employment_type")].into_iter().filter(|x| !x.is_empty()).collect(),
                posted_at: chrono::NaiveDate::parse_from_str(&s(j, "published_on"), "%Y-%m-%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0)).map(|d| d.and_utc()),
                ..job(s(j, "title"), company.clone(), s(j, "url"), places.join(" · "))
            }
        })
        .collect())
}

async fn recruitee(c: &Client, slug: &str) -> anyhow::Result<Vec<RawJob>> {
    let v = json(c, &format!("https://{slug}.recruitee.com/api/offers/")).await?;
    Ok(v["offers"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|j| {
            let mut location = s(j, "location");
            if j["remote"].as_bool() == Some(true) {
                location = format!("Remote · {location}");
            }
            RawJob {
                tags: str_list(j, "tags"),
                posted_at: chrono::NaiveDateTime::parse_from_str(s(j, "published_at").trim_end_matches(" UTC"), "%Y-%m-%d %H:%M:%S").ok().map(|n| n.and_utc()),
                description: plain(&format!("{} {}", s(j, "description"), s(j, "requirements"))),
                ..job(s(j, "title"), s(j, "company_name"), s(j, "careers_url"), location)
            }
        })
        .collect())
}

async fn teamtailor(c: &Client, slug: &str) -> anyhow::Result<Vec<RawJob>> {
    let body = c.get(format!("https://{slug}.teamtailor.com/jobs.rss")).send().await?.error_for_status()?.text().await?;
    anyhow::ensure!(body.contains("<rss"), "no job feed");
    let company = unescape(&xml_field(&body, "title"));
    Ok(ITEM_RE
        .captures_iter(&body)
        .map(|cap| {
            let item = &cap[1];
            let mut places: Vec<String> = Regex::new(r"(?s)<tt:location>(.*?)</tt:location>")
                .unwrap()
                .captures_iter(item)
                .map(|l| [xml_field(&l[1], "tt:city"), xml_field(&l[1], "tt:country")].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(", "))
                .filter(|p| !p.is_empty())
                .collect();
            if xml_field(item, "remoteStatus") == "fully" {
                places.insert(0, "Remote".into());
            }
            RawJob {
                tags: vec![xml_field(item, "tt:department")].into_iter().filter(|x| !x.is_empty()).collect(),
                posted_at: DateTime::parse_from_rfc2822(&xml_field(item, "pubDate")).ok().map(|d| d.with_timezone(&Utc)),
                description: plain(&xml_field(item, "description")),
                ..job(unescape(&xml_field(item, "title")), company.clone(), xml_field(item, "link"), places.join(" · "))
            }
        })
        .collect())
}

async fn personio(c: &Client, slug: &str) -> anyhow::Result<Vec<RawJob>> {
    let body = c.get(format!("https://{slug}.jobs.personio.de/xml")).send().await?.error_for_status()?.text().await?;
    anyhow::ensure!(body.contains("<workzag-jobs"), "no job feed");
    let positions = Regex::new(r"(?s)<position>(.*?)</position>").unwrap();
    let offices = Regex::new(r"(?s)<office>(.*?)</office>").unwrap();
    Ok(positions
        .captures_iter(&body)
        .map(|cap| {
            let p = &cap[1];
            let places: Vec<String> = offices.captures_iter(p).map(|o| unescape(o[1].trim())).collect();
            RawJob {
                tags: [xml_field(p, "department"), xml_field(p, "recruitingCategory")].into_iter().filter(|x| !x.is_empty()).collect(),
                posted_at: rfc3339(&xml_field(p, "createdAt")),
                description: plain(&xml_field(p, "jobDescriptions").replace("<![CDATA[", "").replace("]]>", "")),
                ..job(
                    unescape(&xml_field(p, "name")),
                    unescape(&xml_field(p, "subcompany")),
                    format!("https://{slug}.jobs.personio.de/job/{}", xml_field(p, "id")),
                    places.join(" · "),
                )
            }
        })
        .collect())
}
