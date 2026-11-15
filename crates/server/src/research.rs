//! Web research: search an item's markings on a (self-hosted) SearXNG instance.

use std::{collections::HashSet, time::Duration};

use anyhow::Result;
use reqwest::{Client, Url};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Part-number-like tokens from free-text markings: contain a digit and a letter, at least 3 chars
/// (e.g. `ZS-040`, `HT46R47`, `LM2596S`). Case-insensitively de-duplicated, at most 4.
pub fn part_numbers(markings: &str) -> Vec<String> {
    let mut toks: Vec<String> = Vec::new();
    for t in markings.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '.')) {
        let t = t.trim_matches(|c: char| c == '-' || c == '.');
        let has_digit = t.chars().any(|c| c.is_ascii_digit());
        let has_alpha = t.chars().any(|c| c.is_ascii_alphabetic());
        if t.len() >= 3 && has_digit && has_alpha && !toks.iter().any(|x| x.eq_ignore_ascii_case(t)) {
            toks.push(t.to_string());
        }
    }
    toks.truncate(4);
    toks
}

/// Search queries for the markings: all part numbers together (if several), then the first two individually.
pub fn queries(markings: &str) -> Vec<String> {
    let toks = part_numbers(markings);
    let mut qs = Vec::new();
    if toks.len() > 1 {
        qs.push(toks.iter().take(3).cloned().collect::<Vec<_>>().join(" "));
    }
    for t in toks.iter().take(2) {
        qs.push(format!("{t} module datasheet"));
    }
    qs
}

/// Numbered result list for the model prompt.
pub fn format_for_prompt(hits: &[Hit]) -> String {
    hits.iter().enumerate().map(|(i, h)| format!("{}. {}: {}", i + 1, h.title, h.snippet)).collect::<Vec<_>>().join("\n")
}

/// Markdown source list for the part's notes.
pub fn format_sources(hits: &[Hit]) -> String {
    hits.iter().map(|h| format!("- [{}]({})", h.title.replace(['[', ']'], ""), h.url)).collect::<Vec<_>>().join("\n")
}

pub struct Searx {
    http: Client,
    base: String,
}

impl Searx {
    pub fn new(http: Client, base: String) -> Self {
        Searx { http, base }
    }

    /// Runs all queries, keeps the top 5 per query, de-duplicates by URL, returns at most 10.
    pub async fn search(&self, queries: &[String]) -> Result<Vec<Hit>> {
        let mut seen = HashSet::new();
        let mut hits = Vec::new();
        for q in queries {
            let url = Url::parse_with_params(&format!("{}/search", self.base), &[("q", q.as_str()), ("format", "json")])?;
            let v: Value = self.http.get(url).timeout(Duration::from_secs(30)).send().await?.error_for_status()?.json().await?;
            for r in v["results"].as_array().into_iter().flatten().take(5) {
                let hit = Hit {
                    title: r["title"].as_str().unwrap_or_default().to_string(),
                    url: r["url"].as_str().unwrap_or_default().to_string(),
                    snippet: r["content"].as_str().unwrap_or_default().chars().take(300).collect(),
                };
                if !hit.url.is_empty() && seen.insert(hit.url.clone()) {
                    hits.push(hit);
                }
            }
        }
        hits.truncate(10);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_part_numbers() {
        assert_eq!(part_numbers("HC-05 ZS-040, made in China; VCC GND TXD RXD"), vec!["HC-05", "ZS-040"]);
        assert_eq!(part_numbers("ht46r47 HT46R47 rev.1.2."), vec!["ht46r47", "rev.1.2"]);
        assert!(part_numbers("ARDUINO UNO made in Italy").is_empty());
        assert!(part_numbers("").is_empty());
    }

    #[test]
    fn caps_tokens_and_builds_queries() {
        assert_eq!(part_numbers("A1B B2C C3D D4E E5F").len(), 4);
        assert_eq!(queries("LM2596S"), vec!["LM2596S module datasheet"]);
        assert_eq!(queries("ZY-106C XL6009 B10K"), vec!["ZY-106C XL6009 B10K", "ZY-106C module datasheet", "XL6009 module datasheet"]);
        assert!(queries("no part numbers here").is_empty());
    }

    #[test]
    fn formats_hits() {
        let hits = vec![Hit { title: "HC-05 [datasheet]".into(), url: "https://example.com/hc05".into(), snippet: "Bluetooth SPP".into() }];
        assert_eq!(format_for_prompt(&hits), "1. HC-05 [datasheet]: Bluetooth SPP");
        assert_eq!(format_sources(&hits), "- [HC-05 datasheet](https://example.com/hc05)");
    }
}
