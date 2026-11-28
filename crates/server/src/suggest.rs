//! Box suggestions for "Don't know" captures.
//!
//! Boxes are first ranked by what they already hold (same InvenTree category, similar part names, a box name
//! that matches the item). The model then picks from that shortlist and says why; the ranking alone is the
//! fallback, and also tops the model's picks up to `MAX`.

use std::collections::HashSet;

use serde::Deserialize;

use crate::{inventree::BoxContents, spool::Suggestion};

/// Suggestions per item.
pub const MAX: usize = 3;
/// Boxes shown to the model.
const SHORTLIST: usize = 12;
/// Example part names per box in the prompt.
const EXAMPLES: usize = 8;

const STOP: &[&str] =
    &["and", "for", "the", "with", "module", "board", "inbox", "pcs", "set", "kit", "mini", "small", "large", "type", "pack"];

/// Lower-case words of 3+ chars (or containing a digit, e.g. "5v"), minus filler words, with a plural "s"
/// dropped so "cables" matches "cable".
fn words(s: &str) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| (w.len() >= 3 || w.chars().any(|c| c.is_ascii_digit())) && !STOP.contains(&w.as_str()))
        .map(|w| if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") { w[..w.len() - 1].to_string() } else { w })
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ranked<'a> {
    pub b: &'a BoxContents,
    pub same_category: usize,
    pub similar: Vec<&'a str>,
    pub name_match: bool,
}

impl Ranked<'_> {
    fn score(&self) -> usize {
        3 * self.same_category + 2 * self.similar.len() + if self.name_match { 4 } else { 0 }
    }

    fn reason(&self) -> String {
        let mut r = Vec::new();
        if self.name_match {
            r.push("box name matches".to_string());
        }
        if self.same_category > 0 {
            r.push(format!("{} item(s) in the same category", self.same_category));
        }
        if !self.similar.is_empty() {
            r.push(format!("similar: {}", self.similar.iter().take(2).copied().collect::<Vec<_>>().join(", ")));
        }
        if r.is_empty() { "holds related items".into() } else { r.join("; ") }
    }
}

/// Best boxes first. Boxes with no evidence are kept (after the rest) so the model can still go by box
/// names and contents, but empty boxes are dropped.
pub fn shortlist<'a>(boxes: &'a [BoxContents], name: &str, category: Option<i64>) -> Vec<Ranked<'a>> {
    let want = words(name);
    let mut v: Vec<Ranked> = boxes
        .iter()
        .filter(|b| !b.parts.is_empty())
        .map(|b| {
            let same_category = category.map_or(0, |c| b.parts.iter().filter(|(_, pc)| *pc == Some(c)).count());
            let mut similar: Vec<&str> = b.parts.iter().map(|(n, _)| n.as_str()).filter(|n| !words(n).is_disjoint(&want)).collect();
            similar.dedup();
            let name_match = !words(&b.name).is_disjoint(&want);
            Ranked { b, same_category, similar, name_match }
        })
        .collect();
    v.sort_by(|a, b| b.score().cmp(&a.score()).then(b.b.parts.len().cmp(&a.b.parts.len())));
    v.truncate(SHORTLIST);
    v
}

pub fn prompt(name: &str, description: &str, list: &[Ranked]) -> String {
    let mut boxes = String::new();
    for r in list {
        let mut seen = HashSet::new();
        let examples: Vec<&str> = r.b.parts.iter().map(|(n, _)| n.as_str()).filter(|n| seen.insert(*n)).take(EXAMPLES).collect();
        boxes.push_str(&format!("- \"{}\" ({} items), e.g.: {}\n", r.b.name, r.b.parts.len(), examples.join("; ")));
    }
    format!(
        "You help a hobbyist decide which storage box a newly found item belongs in.\n\
         The item: {name}\nDescription: {description}\n\
         Existing boxes and some of what they already hold:\n{boxes}\
         Choose up to {MAX} boxes where this item fits best, best first: boxes that hold the same kind of thing. \
         Use box names exactly as written above.\n\
         Reply with ONLY a JSON object:\n\
         {{\"boxes\": [{{\"box\": \"exact box name\", \"reason\": \"few words: why it fits\"}}]}}"
    )
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    boxes: Vec<Pick>,
}

#[derive(Deserialize)]
struct Pick {
    #[serde(rename = "box", default)]
    name: String,
    #[serde(default)]
    reason: String,
}

/// The model's picks that name a shortlisted box (anything else is ignored), topped up from the ranking.
pub fn choose(reply: Option<&str>, list: &[Ranked]) -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = Vec::new();
    let picks = reply.and_then(|r| serde_json::from_str::<Reply>(r).ok()).map(|r| r.boxes).unwrap_or_default();
    for p in picks {
        let n = p.name.trim().trim_matches('"');
        let Some(r) = list.iter().find(|r| r.b.name.eq_ignore_ascii_case(n)) else { continue };
        if out.len() < MAX && !out.iter().any(|s| s.location == r.b.pk) {
            let reason: String = p.reason.trim().chars().take(120).collect();
            out.push(Suggestion { location: r.b.pk, name: r.b.name.clone(), reason: if reason.is_empty() { r.reason() } else { reason } });
        }
    }
    for r in list.iter().filter(|r| r.score() > 0) {
        if out.len() < MAX && !out.iter().any(|s| s.location == r.b.pk) {
            out.push(Suggestion { location: r.b.pk, name: r.b.name.clone(), reason: r.reason() });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(pk: i64, name: &str, parts: &[(&str, Option<i64>)]) -> BoxContents {
        BoxContents { pk, name: name.into(), parts: parts.iter().map(|(n, c)| (n.to_string(), *c)).collect() }
    }

    fn boxes() -> Vec<BoxContents> {
        vec![
            bx(1, "A1", &[("ESP32 dev board", Some(10)), ("Arduino Nano", Some(10))]),
            bx(2, "B2/cables", &[("USB-A to micro-USB cable", Some(20)), ("Dupont jumper wires", Some(20))]),
            bx(3, "C3", &[("LM2596 buck converter", Some(30))]),
            bx(4, "EMPTY", &[]),
        ]
    }

    #[test]
    fn ranks_by_category_names_and_box_name() {
        let b = boxes();
        let l = shortlist(&b, "USB-C cable", Some(20));
        assert_eq!(l[0].b.pk, 2);
        assert_eq!(l[0].same_category, 2);
        assert!(l[0].name_match);
        assert_eq!(l[0].similar, vec!["USB-A to micro-USB cable"]);
        assert!(l.iter().all(|r| r.b.pk != 4), "empty boxes are not suggested");
    }

    #[test]
    fn model_picks_are_validated_and_topped_up() {
        let b = boxes();
        let l = shortlist(&b, "ESP8266 dev board", Some(10));
        let s = choose(Some(r#"{"boxes":[{"box":"c3","reason":"power stuff"},{"box":"Z9","reason":"made up"},{"box":"C3"}]}"#), &l);
        assert_eq!(s.iter().map(|x| x.location).collect::<Vec<_>>(), vec![3, 1]);
        assert_eq!(s[0].reason, "power stuff");
        assert!(s[1].reason.contains("same category"));
    }

    #[test]
    fn falls_back_to_ranking_when_the_model_fails() {
        let b = boxes();
        let l = shortlist(&b, "Arduino Uno", Some(10));
        let s = choose(Some("not json"), &l);
        assert_eq!(s[0].location, 1);
        assert!(choose(None, &shortlist(&b, "spaceship", None)).is_empty());
    }

    #[test]
    fn prompt_lists_boxes_and_examples() {
        let b = boxes();
        let p = prompt("USB-C cable", "a cable", &shortlist(&b, "USB-C cable", Some(20)));
        assert!(p.contains("\"B2/cables\" (2 items), e.g.: USB-A to micro-USB cable; Dupont jumper wires"));
        assert!(p.contains("USB-C cable") && !p.contains("EMPTY"));
    }
}
