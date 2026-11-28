//! Identification with a local vision model served by Ollama.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use base64::Engine;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

const CATEGORIES: &str = "microcontroller, computer, wireless, networking, interface, actuator, power, sensor, display, input, component, cable, tool, hardware, kit, misc, unknown";

const REPLY_FORMAT: &str = r#"Reply with ONLY a JSON object:
{"markings": "text you can read", "name": "short specific name, max 80 chars", "description": "one sentence: what it is / what it's for (mention key specs if known)", "category": "one of: {categories}", "confidence": "high|medium|low"}"#;

/// How the photos are introduced: one photo, or several of the same item.
fn photos_intro(n: usize) -> String {
    match n {
        0 | 1 => "from a phone photo".into(),
        n => format!(
            "from {n} phone photos of the SAME single item, taken from different sides (e.g. front, back, label). \
             Combine what you see in all of them; a marking may be readable in only one photo"
        ),
    }
}

/// First look at the photo(s).
pub fn identify_prompt(photos: usize) -> String {
    format!(
        "You are cataloguing a hobbyist's box of electronics and maker parts {}.\n\
         First read every printed marking, label, chip number, brand and model you can see.\n\
         Then identify the item as specifically as you can (e.g. \"HC-05 Bluetooth serial module (ZS-040)\", \
         \"LM2596 buck converter module\", \"USB-A to micro-USB cable\").\n{}",
        photos_intro(photos),
        REPLY_FORMAT.replace("{categories}", CATEGORIES)
    )
}

/// Second look, grounded in web search results for the markings the first look read.
pub fn research_prompt(photos: usize, markings: &str, guess: &str, results: &str) -> String {
    format!(
        "You are cataloguing a hobbyist's electronics and maker parts {}.\n\
         An earlier look was unsure. Markings read from the item: {markings}\n\
         Earlier guess (may be wrong, small model): {guess}\n\
         Web search results for those markings:\n{results}\n\
         Using the photo(s) AND the search results, identify the item as specifically as you can. \
         Ignore results that do not match the photo(s).\n\
         Prefer what the search results say a marking/part number is over the earlier guess. \
         Use \"high\" confidence only if a search result clearly matches the markings AND the photo(s).\n{}",
        photos_intro(photos),
        REPLY_FORMAT.replace("{categories}", CATEGORIES)
    )
}

/// What the model thinks the item is.
#[derive(Deserialize, Debug, Clone, PartialEq)]
pub struct Guess {
    #[serde(default)]
    pub markings: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub confidence: String,
}

impl Guess {
    pub fn parse(content: &str) -> Result<Self> {
        let g: Guess =
            serde_json::from_str(content).with_context(|| format!("bad model json: {}", content.chars().take(300).collect::<String>()))?;
        anyhow::ensure!(!g.name.trim().is_empty(), "model returned an empty name");
        Ok(g)
    }

    /// Normalised confidence: "high", "medium", "low" (anything else counts as low).
    pub fn confidence(&self) -> &'static str {
        match self.confidence.trim().to_ascii_lowercase().as_str() {
            "high" => "high",
            "medium" => "medium",
            _ => "low",
        }
    }
}

/// The photos as the model sees them: base64 JPEGs of at most 1024 px, or 768 px when there are more than two,
/// which keeps vision tokens (and latency) down and several photos inside the context window.
pub fn model_images(paths: &[PathBuf]) -> Result<Vec<String>> {
    let max = if paths.len() > 2 { 768 } else { 1024 };
    paths.iter().map(|p| model_image(p, max)).collect()
}

fn model_image(path: &Path, max: u32) -> Result<String> {
    let img = image::open(path).with_context(|| format!("reading {}", path.display()))?;
    let img = if img.width().max(img.height()) > max { img.resize(max, max, image::imageops::FilterType::Triangle) } else { img };
    let mut buf = std::io::Cursor::new(Vec::new());
    img.to_rgb8().write_to(&mut buf, image::ImageFormat::Jpeg)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(buf.into_inner()))
}

/// A small JPEG for the capture log (the full photo is deleted once identification is final).
pub fn thumbnail(photo: &[u8]) -> Result<Vec<u8>> {
    let img = image::load_from_memory(photo)?.thumbnail(200, 200);
    let mut buf = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 70).encode_image(&img.to_rgb8())?;
    Ok(buf.into_inner())
}

pub struct Vision {
    http: Client,
    url: String,
    pub model: String,
}

impl Vision {
    pub fn new(http: Client, url: String, model: String) -> Self {
        Vision { http, url, model }
    }

    pub async fn ask(&self, prompt: &str, images: &[String]) -> Result<Guess> {
        let images: Vec<&str> = images.iter().map(String::as_str).collect();
        Guess::parse(&self.chat(prompt, &images).await?)
    }

    /// One JSON-mode chat turn; returns the raw content.
    pub async fn chat(&self, prompt: &str, images: &[&str]) -> Result<String> {
        let req = json!({
            "model": self.model,
            "stream": false,
            "think": false,
            "format": "json",
            // num_predict caps runaway generations (seen: 1400+ tokens of rambling from small models)
            "options": {"temperature": 0.2, "num_predict": 400, "num_ctx": 8192},
            "messages": [{"role": "user", "content": prompt, "images": images}],
        });
        let resp: Value = self
            .http
            .post(format!("{}/api/chat", self.url))
            .timeout(Duration::from_secs(240))
            .json(&req)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp["message"]["content"].as_str().context("no content from model")?.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_model_reply() {
        let g = Guess::parse(
            r#"{"markings":"ZS-040 HC-05","name":"HC-05 Bluetooth serial module","description":"Classic BT SPP module","category":"wireless","confidence":"High"}"#,
        )
        .unwrap();
        assert_eq!(g.name, "HC-05 Bluetooth serial module");
        assert_eq!(g.confidence(), "high");
    }

    #[test]
    fn missing_optional_fields_default() {
        let g = Guess::parse(r#"{"name":"USB cable"}"#).unwrap();
        assert_eq!(g.confidence(), "low");
        assert!(g.markings.is_empty());
    }

    #[test]
    fn rejects_empty_name_and_non_json() {
        assert!(Guess::parse(r#"{"name":"  "}"#).is_err());
        assert!(Guess::parse("I think this is an Arduino").is_err());
    }

    #[test]
    fn prompts_embed_categories_and_context() {
        assert!(identify_prompt(1).contains("microcontroller, computer"));
        assert!(identify_prompt(1).contains("from a phone photo."));
        assert!(identify_prompt(3).contains("3 phone photos of the SAME single item"));
        let p = research_prompt(2, "HT46R47", "Bluetooth module", "1. HT46R47 datasheet: 8-bit MCU");
        assert!(p.contains("2 phone photos"));
        assert!(p.contains("HT46R47") && p.contains("Bluetooth module") && p.contains("8-bit MCU"));
        assert!(!p.contains("{categories}"));
    }
}
