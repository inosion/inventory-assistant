//! Minimal InvenTree REST client (token auth): just what the capture pipeline needs.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use reqwest::{Client, RequestBuilder, Url, multipart};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

pub const INBOX: &str = "Inbox";

pub struct Inv {
    http: Client,
    base: String,
    token: String,
    cats: Mutex<HashMap<String, i64>>,
}

#[derive(Serialize)]
pub struct Loc {
    pk: i64,
    name: String,
    items: i64,
}

/// AI category slug -> InvenTree category path (kept in step with `tools/yaml-import`).
/// `None` (e.g. "unknown") leaves the part in the Inbox category.
pub fn category_for(slug: &str) -> Option<&'static str> {
    Some(match slug.trim().to_lowercase().as_str() {
        "microcontroller" => "Microcontrollers",
        "computer" => "Single-board computers",
        "wireless" => "Wireless",
        "networking" => "Networking",
        "interface" => "Interfaces & adapters",
        "actuator" => "Actuators & relays",
        "power" => "Power",
        "sensor" => "Sensors",
        "display" => "Displays",
        "input" => "Input",
        "component" => "Components",
        "cable" => "Cables",
        "tool" => "Tools",
        "hardware" => "Hardware",
        "kit" => "Kits",
        "misc" => "Misc",
        _ => return None,
    })
}

fn results(v: Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a,
        Value::Object(mut o) => match o.remove("results") {
            Some(Value::Array(a)) => a,
            _ => vec![],
        },
        _ => vec![],
    }
}

fn pk(v: &Value) -> Result<i64> {
    v["pk"].as_i64().with_context(|| format!("no pk in {v}"))
}

impl Inv {
    pub fn new(http: Client, base: String, token: String) -> Self {
        Inv { http, base: base.trim_end_matches('/').to_string(), token, cats: Mutex::new(HashMap::new()) }
    }

    fn url(&self, path: &str, q: &[(&str, String)]) -> Result<Url> {
        Ok(Url::parse_with_params(&format!("{}/api/{}", self.base, path.trim_start_matches('/')), q)?)
    }

    async fn send(&self, rb: RequestBuilder) -> Result<Value> {
        let r = rb.header("Authorization", format!("Token {}", self.token)).header("Accept", "application/json").send().await?;
        let status = r.status();
        let text = r.text().await?;
        if !status.is_success() {
            bail!("InvenTree {status}: {}", text.chars().take(400).collect::<String>());
        }
        Ok(if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text)? })
    }

    async fn get(&self, path: &str, q: &[(&str, String)]) -> Result<Value> {
        self.send(self.http.get(self.url(path, q)?)).await
    }
    async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.send(self.http.post(self.url(path, &[])?).json(body)).await
    }
    async fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        self.send(self.http.patch(self.url(path, &[])?).json(body)).await
    }

    pub async fn list_locations(&self) -> Result<Vec<Loc>> {
        let v = results(self.get("stock/location/", &[("limit", "1000".into())]).await?);
        let mut locs: Vec<Loc> = v
            .iter()
            .filter_map(|l| {
                Some(Loc {
                    pk: l["pk"].as_i64()?,
                    name: l["pathstring"].as_str().or(l["name"].as_str())?.to_string(),
                    items: l["items"].as_i64().unwrap_or(0),
                })
            })
            .collect();
        locs.sort_by_key(|l| l.name.to_lowercase());
        Ok(locs)
    }

    pub async fn location_path(&self, pk: i64) -> Result<String> {
        let l = self.get(&format!("stock/location/{pk}/"), &[]).await?;
        Ok(l["pathstring"].as_str().unwrap_or_default().to_string())
    }

    /// Finds or creates a (possibly nested, "B1/cables") tree path under `endpoint`.
    async fn ensure_tree(&self, endpoint: &str, path: &str) -> Result<i64> {
        let mut parent: Option<i64> = None;
        for seg in path.split('/').map(str::trim).filter(|s| !s.is_empty()) {
            let mut q = vec![("name", seg.to_string())];
            match parent {
                Some(p) => q.push(("parent", p.to_string())),
                None => q.push(("top_level", "true".into())),
            }
            let found = results(self.get(endpoint, &q).await?);
            // the name filter is exact, but be defensive about case
            let hit = found.iter().find(|x| x["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(seg)));
            parent = Some(match hit {
                Some(x) => pk(x)?,
                None => pk(&self.post(endpoint, &json!({"name": seg, "parent": parent})).await?)?,
            });
        }
        parent.context("empty name")
    }

    pub async fn ensure_location(&self, path: &str) -> Result<i64> {
        self.ensure_tree("stock/location/", path).await
    }

    pub async fn ensure_category(&self, path: &str) -> Result<i64> {
        if let Some(pk) = self.cats.lock().await.get(path) {
            return Ok(*pk);
        }
        let pk = self.ensure_tree("part/category/", path).await?;
        self.cats.lock().await.insert(path.to_string(), pk);
        Ok(pk)
    }

    /// Creates (or finds, by IPN, for retries) the inbox part, its image and its stock.
    pub async fn create_inbox_part(&self, ipn: &str, name: &str, location: i64, qty: f64, photo: Vec<u8>) -> Result<i64> {
        let inbox = self.ensure_category(INBOX).await?;
        let existing = results(self.get("part/", &[("IPN", ipn.into())]).await?);
        let part = match existing.into_iter().next() {
            Some(p) => p,
            None => {
                self.post(
                    "part/",
                    &json!({
                        "name": name, "IPN": ipn,
                        "description": "Quick capture - awaiting identification",
                        "category": inbox, "default_location": location,
                        "component": true, "purchaseable": true,
                    }),
                )
                .await?
            }
        };
        let id = pk(&part)?;
        if part["image"].is_null() {
            let form =
                multipart::Form::new().part("image", multipart::Part::bytes(photo).file_name(format!("{ipn}.jpg")).mime_str("image/jpeg")?);
            self.send(self.http.patch(self.url(&format!("part/{id}/"), &[])?).multipart(form)).await?;
        }
        if results(self.get("stock/", &[("part", id.to_string())]).await?).is_empty() {
            self.post("stock/", &json!({"part": id, "location": location, "quantity": qty})).await?;
        }
        Ok(id)
    }

    pub async fn apply_guess(&self, part: i64, name: &str, desc: &str, notes: &str, category: Option<&str>) -> Result<()> {
        let mut body = json!({
            "name": name.chars().take(100).collect::<String>(),
            "description": desc.chars().take(250).collect::<String>(),
            "notes": notes,
        });
        if let Some(c) = category {
            body["category"] = json!(self.ensure_category(c).await?);
        }
        self.patch(&format!("part/{part}/"), &body).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_categories_only() {
        assert_eq!(category_for(" Microcontroller "), Some("Microcontrollers"));
        assert_eq!(category_for("interface"), Some("Interfaces & adapters"));
        assert_eq!(category_for("unknown"), None);
        assert_eq!(category_for("spaceship"), None);
    }

    #[test]
    fn unwraps_paginated_and_plain_lists() {
        assert_eq!(results(json!([{"pk": 1}])).len(), 1);
        assert_eq!(results(json!({"count": 2, "results": [{"pk": 1}, {"pk": 2}]})).len(), 2);
        assert!(results(json!({"detail": "nope"})).is_empty());
        assert_eq!(pk(&json!({"pk": 7})).unwrap(), 7);
        assert!(pk(&json!({})).is_err());
    }
}
