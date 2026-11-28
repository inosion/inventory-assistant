//! Is a new capture a part that is already in InvenTree?
//!
//! Candidates come from InvenTree's part search (on the AI name and on part numbers read from the item).
//! An identical name is a match; otherwise the model compares the item with the candidates and only a
//! `high` confidence answer counts. A wrong merge is worse than a duplicate part, so anything unsure is
//! left alone.

use serde::Deserialize;

use crate::inventree::PartLite;

/// Lower-case alphanumeric words, single-spaced: "HC-05  Bluetooth module" == "hc 05 bluetooth module".
fn normalize(s: &str) -> String {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect::<Vec<_>>().join(" ")
}

/// A candidate with exactly the same (normalised) name.
pub fn exact(name: &str, cands: &[PartLite]) -> Option<usize> {
    let n = normalize(name);
    (!n.is_empty()).then(|| cands.iter().position(|c| normalize(&c.name) == n)).flatten()
}

pub fn prompt(name: &str, description: &str, markings: &str, cands: &[PartLite]) -> String {
    let list: String = cands.iter().enumerate().map(|(i, c)| format!("{}. {}: {}\n", i + 1, c.name, c.description)).collect();
    format!(
        "A hobbyist photographed an item to add to their parts inventory.\n\
         Identified as: {name}\nDescription: {description}\nMarkings read: {markings}\n\
         Parts already in the inventory:\n{list}\
         Is the item the SAME product as one of these (same model / part number, not merely the same kind of thing)?\n\
         Reply with ONLY a JSON object: {{\"match\": <number of the matching part, or 0 if none>, \"confidence\": \"high|medium|low\"}}"
    )
}

#[derive(Deserialize)]
struct Reply {
    #[serde(rename = "match", default)]
    m: serde_json::Value,
    #[serde(default)]
    confidence: String,
}

/// The matching candidate's index, only for a confident answer naming a listed part.
pub fn parse(reply: &str, n: usize) -> Option<usize> {
    let r: Reply = serde_json::from_str(reply).ok()?;
    if !r.confidence.trim().eq_ignore_ascii_case("high") {
        return None;
    }
    let i = r.m.as_u64().or_else(|| r.m.as_str()?.trim().parse().ok())? as usize;
    (1..=n).contains(&i).then(|| i - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands() -> Vec<PartLite> {
        vec![
            PartLite { pk: 1, name: "HC-05 Bluetooth serial module".into(), description: "BT SPP".into() },
            PartLite { pk: 2, name: "LM2596 buck converter module".into(), description: "DC-DC step down".into() },
        ]
    }

    #[test]
    fn exact_name_ignores_case_and_punctuation() {
        assert_eq!(exact("hc 05  Bluetooth Serial module", &cands()), Some(0));
        assert_eq!(exact("HC-06 Bluetooth serial module", &cands()), None);
        assert_eq!(exact(" - ", &cands()), None);
    }

    #[test]
    fn only_confident_answers_for_listed_parts_count() {
        assert_eq!(parse(r#"{"match": 2, "confidence": "High"}"#, 2), Some(1));
        assert_eq!(parse(r#"{"match": "1", "confidence": "high"}"#, 2), Some(0));
        assert_eq!(parse(r#"{"match": 2, "confidence": "medium"}"#, 2), None);
        assert_eq!(parse(r#"{"match": 0, "confidence": "high"}"#, 2), None);
        assert_eq!(parse(r#"{"match": 3, "confidence": "high"}"#, 2), None);
        assert_eq!(parse("nope", 2), None);
    }

    #[test]
    fn prompt_numbers_candidates() {
        let p = prompt("LM2596 module", "buck", "LM2596S", &cands());
        assert!(p.contains("1. HC-05 Bluetooth serial module: BT SPP\n2. LM2596 buck converter"));
        assert!(p.contains("LM2596S"));
    }
}
