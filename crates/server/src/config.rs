//! Runtime configuration, read once from environment variables.

use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result};

/// When to web-search an item's markings after the first identification pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResearchPolicy {
    /// Research every item whose markings contain part-number-like tokens (default).
    /// Small vision models are often confidently wrong, so this is the safer choice.
    Markings,
    /// Only research items the model itself marked as low confidence.
    Low,
    /// Never research.
    Off,
}

impl ResearchPolicy {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Self::Low,
            "off" | "none" | "never" => Self::Off,
            _ => Self::Markings,
        }
    }
}

pub struct Config {
    pub listen: SocketAddr,
    pub data_dir: PathBuf,
    pub dist_dir: PathBuf,
    pub inventree_url: String,
    pub inventree_token: String,
    pub ollama_url: String,
    pub model: String,
    pub searxng_url: Option<String>,
    pub research: ResearchPolicy,
}

fn env(k: &str, default: &str) -> String {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| default.to_string())
}

fn trim_url(s: String) -> String {
    s.trim_end_matches('/').to_string()
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Config {
            listen: env("LISTEN", "0.0.0.0:3000").parse().context("LISTEN must be host:port")?,
            data_dir: PathBuf::from(env("DATA_DIR", "./spool")),
            dist_dir: PathBuf::from(env("DIST_DIR", "./crates/web/dist")),
            inventree_url: trim_url(env("INVENTREE_URL", "http://inventree-server:8000")),
            inventree_token: std::env::var("INVENTREE_TOKEN").context("INVENTREE_TOKEN is not set")?,
            ollama_url: trim_url(env("OLLAMA_URL", "http://localhost:11434")),
            model: env("OLLAMA_MODEL", "qwen3.5:4b"),
            searxng_url: std::env::var("SEARXNG_URL").ok().filter(|s| !s.trim().is_empty()).map(trim_url),
            research: ResearchPolicy::parse(&env("RESEARCH_POLICY", "markings")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn research_policy_parsing() {
        assert_eq!(ResearchPolicy::parse("markings"), ResearchPolicy::Markings);
        assert_eq!(ResearchPolicy::parse(" LOW "), ResearchPolicy::Low);
        assert_eq!(ResearchPolicy::parse("off"), ResearchPolicy::Off);
        assert_eq!(ResearchPolicy::parse("anything-else"), ResearchPolicy::Markings);
    }
}
