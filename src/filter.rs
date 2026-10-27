//! Decides whether a job can be done from Brazil and how well it matches the profile.

use regex::Regex;
use serde::Serialize;

use crate::config::Profile;
use crate::sources::RawJob;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Eligibility {
    Yes,
    Maybe,
    No,
}

impl Eligibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Eligibility::Yes => "yes",
            Eligibility::Maybe => "maybe",
            Eligibility::No => "no",
        }
    }
}

/// Location words that include someone living in Brazil.
const OPEN_TO_BRAZIL: &[&str] = &[
    "brazil", "brasil", "latam", "latin america", "south america", "americas", "worldwide",
    "anywhere", "global", "utc-3", "gmt-3", "work from anywhere",
];

/// Description phrases strong enough to count as an explicit opening for Brazil.
/// Words like "anywhere" or "global" show up in company blurbs too often to trust here.
const TEXT_OPEN: &[&str] = &[
    "brazil", "brasil", "latam", "latin america", "south america", "anywhere in the world",
    "work from anywhere", "worldwide", "utc-3", "gmt-3",
];

/// Phrases in a posting that rule out a Brazil-based candidate.
const BLOCKERS: &[&str] = &[
    "us only", "usa only", "u.s. only", "united states only", "us-based only", "us based only",
    "must be based in the us", "must be located in the us", "must reside in the us",
    "must be based in the united states", "must reside in the united states",
    "authorized to work in the united states", "authorized to work in the us",
    "authorization to work in the united states", "us citizen", "u.s. citizen",
    "security clearance", "eu only", "europe only", "uk only", "canada only",
    "must be based in europe", "must be located in europe", "right to work in the uk",
    "eu work permit", "valid eu work authorization", "remote - usa", "remote (us)", "remote, us",
    "usa / canada", "us / canada", "us/canada", "u.s. or canada", "us or canada",
];

/// Location noise that says nothing about where you may live.
const LOCATION_NOISE: &[&str] = &["remote", "fully", "100%", "hybrid", "home", "based", "-", "/", "(", ")", ",", "|"];

pub struct Matcher {
    title_keywords: Vec<Regex>,
    exclude_title: Vec<Regex>,
    seniority: Vec<Regex>,
    skills: Vec<(String, Regex, i32)>,
}

fn word(kw: &str) -> Regex {
    Regex::new(&format!(r"(?i)(^|[^a-z0-9]){}($|[^a-z0-9])", regex::escape(kw))).expect("valid keyword regex")
}

pub struct Verdict {
    pub eligibility: Eligibility,
    pub reason: String,
    pub score: i32,
    pub matched: Vec<String>,
}

impl Matcher {
    pub fn new(p: &Profile) -> Self {
        Self {
            title_keywords: p.title_keywords.iter().map(|k| word(k)).collect(),
            exclude_title: p.exclude_title.iter().map(|k| word(k)).collect(),
            seniority: p.seniority.iter().map(|k| word(k)).collect(),
            skills: p.skills.iter().map(|(k, w)| (k.clone(), word(k), *w)).collect(),
        }
    }

    /// Returns None when the title isn't a software or AI engineering role.
    pub fn judge(&self, job: &RawJob) -> Option<Verdict> {
        let title = &job.title;
        if !self.title_keywords.iter().any(|r| r.is_match(title)) {
            return None;
        }
        if self.exclude_title.iter().any(|r| r.is_match(title)) {
            return None;
        }

        let (eligibility, reason) = classify(&job.location, &job.description);

        let haystack = format!("{} {}", job.tags.join(" "), job.description);
        let mut score = 0;
        let mut matched = Vec::new();
        for (name, re, weight) in &self.skills {
            let in_title = re.is_match(title);
            if in_title || re.is_match(&haystack) {
                score += if in_title { weight * 2 } else { *weight };
                matched.push(name.clone());
            }
        }
        if self.seniority.iter().any(|r| r.is_match(title)) {
            score += 3;
        }
        score += match eligibility {
            Eligibility::Yes => 5,
            Eligibility::Maybe => 0,
            Eligibility::No => -20,
        };

        Some(Verdict { eligibility, reason, score, matched })
    }
}

pub fn classify(location: &str, description: &str) -> (Eligibility, String) {
    let loc = location.to_lowercase();
    let text = description.to_lowercase();

    let loc_open = OPEN_TO_BRAZIL.iter().find(|w| loc.contains(*w));
    let names_brazil = loc.contains("brazil") || loc.contains("brasil") || loc.contains("latam") || loc.contains("latin america");

    if let Some(b) = BLOCKERS.iter().find(|b| loc.contains(*b) || text.contains(*b)) {
        if !names_brazil {
            return (Eligibility::No, format!("Posting says \"{b}\""));
        }
    }

    if let Some(w) = loc_open {
        return (Eligibility::Yes, format!("Location open to Brazil ({w})"));
    }

    let mut rest = loc.clone();
    for n in LOCATION_NOISE {
        rest = rest.replace(n, " ");
    }
    if rest.trim().is_empty() {
        if let Some(w) = TEXT_OPEN.iter().find(|w| text.contains(*w)) {
            return (Eligibility::Yes, format!("Description mentions {w}"));
        }
        return (Eligibility::Maybe, "Remote, no country rule stated".into());
    }

    if let Some(w) = TEXT_OPEN.iter().find(|w| text.contains(*w)) {
        return (Eligibility::Maybe, format!("Listed for {}, but the description mentions {w}", location.trim()));
    }
    (Eligibility::No, format!("Restricted to {}", location.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_locations() {
        assert_eq!(classify("Worldwide", "").0, Eligibility::Yes);
        assert_eq!(classify("LATAM", "").0, Eligibility::Yes);
        assert_eq!(classify("Remote", "").0, Eligibility::Maybe);
        assert_eq!(classify("", "").0, Eligibility::Maybe);
        assert_eq!(classify("Remote, United States", "").0, Eligibility::No);
        assert_eq!(classify("Anywhere", "You must be authorized to work in the United States").0, Eligibility::No);
        assert_eq!(classify("Brazil", "US citizens welcome").0, Eligibility::Yes);
        assert_eq!(classify("Remote", "We hire across Latin America").0, Eligibility::Yes);
        assert_eq!(classify("Remote", "Build from anywhere. Senior Engineer (Remote - USA / Canada)").0, Eligibility::No);
        assert_eq!(classify("Remote", "A global company serving customers anywhere").0, Eligibility::Maybe);
    }
}
