//! Minimal InvenTree REST client (token auth): just what the capture pipeline needs.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use reqwest::{Client, RequestBuilder, Url, multipart};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// Part category for unidentified parts, and stock location for "Don't know" captures awaiting a box.
pub const INBOX: &str = "Inbox";

pub struct Inv {
    http: Client,
    base: String,
    token: String,
    cats: Mutex<HashMap<String, i64>>,
}

#[derive(Serialize)]
pub struct Loc {
    pub pk: i64,
    pub name: String,
    pub items: i64,
}

/// A part as the duplicate check sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct PartLite {
    pub pk: i64,
    pub name: String,
    pub description: String,
}

/// What is already in a box, used to suggest a box for a new item.
#[derive(Debug, Clone, PartialEq)]
pub struct BoxContents {
    pub pk: i64,
    pub name: String,
    /// (part name, part category pk) per stock item.
    pub parts: Vec<(String, Option<i64>)>,
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

/// Creating stock returns the item, or (some InvenTree versions) a list of the items created.
fn stock_pk(v: Value) -> Result<i64> {
    match &v {
        Value::Array(a) => pk(a.first().context("no stock item created")?),
        _ => pk(&v),
    }
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
    async fn delete(&self, path: &str) -> Result<Value> {
        self.send(self.http.delete(self.url(path, &[])?)).await
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
    /// Returns the part and stock item pks.
    pub async fn create_inbox_part(&self, ipn: &str, name: &str, location: i64, qty: f64, photo: Vec<u8>) -> Result<(i64, i64)> {
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
        let stock = match results(self.get("stock/", &[("part", id.to_string())]).await?).first() {
            Some(s) => pk(s)?,
            None => stock_pk(self.post("stock/", &json!({"part": id, "location": location, "quantity": qty})).await?)?,
        };
        Ok((id, stock))
    }

    /// A part's category pk and description.
    pub async fn part_info(&self, part: i64) -> Result<(Option<i64>, String)> {
        let p = self.get(&format!("part/{part}/"), &[]).await?;
        Ok((p["category"].as_i64(), p["description"].as_str().unwrap_or_default().to_string()))
    }

    /// Every box with the parts stocked in it (all boxes, even empty ones), except `exclude`.
    pub async fn box_contents(&self, exclude: i64) -> Result<Vec<BoxContents>> {
        let mut boxes: Vec<BoxContents> = self
            .list_locations()
            .await?
            .into_iter()
            .filter(|l| l.pk != exclude)
            .map(|l| BoxContents { pk: l.pk, name: l.name, parts: vec![] })
            .collect();
        let q = [("limit", "5000".to_string()), ("part_detail", "true".into()), ("in_stock", "true".into())];
        for s in results(self.get("stock/", &q).await?) {
            let Some(b) = s["location"].as_i64().and_then(|l| boxes.iter_mut().find(|b| b.pk == l)) else { continue };
            let p = &s["part_detail"];
            if let Some(name) = p["full_name"].as_str().or(p["name"].as_str()) {
                b.parts.push((name.to_string(), p["category"].as_i64()));
            }
        }
        Ok(boxes)
    }

    /// Moves a stock item (all of it) into `location` and makes that the part's default location. If the
    /// part already has stock there, the quantity is added to that item instead, so the box has one entry.
    pub async fn move_stock(&self, stock: i64, part: i64, location: i64, note: &str) -> Result<()> {
        let item = self.get(&format!("stock/{stock}/"), &[]).await?;
        if item["location"].as_i64() != Some(location) {
            let qty = item["quantity"].as_f64().context("stock item has no quantity")?;
            let there = self.stock_at(part, location).await?.into_iter().find(|s| *s != stock);
            match there {
                Some(existing) => {
                    self.post("stock/add/", &json!({"items": [{"pk": existing, "quantity": qty}], "notes": note})).await?;
                    self.delete(&format!("stock/{stock}/")).await?;
                }
                None => {
                    self.post("stock/transfer/", &json!({"items": [{"pk": stock, "quantity": qty}], "location": location, "notes": note}))
                        .await?;
                }
            }
        }
        self.patch(&format!("part/{part}/"), &json!({"default_location": location})).await?;
        Ok(())
    }

    /// Stock item pks of `part` at `location`.
    async fn stock_at(&self, part: i64, location: i64) -> Result<Vec<i64>> {
        let q = [("part", part.to_string()), ("location", location.to_string()), ("cascade", "false".into())];
        results(self.get("stock/", &q).await?).iter().map(pk).collect()
    }

    /// Locations holding stock of `part`.
    pub async fn part_locations(&self, part: i64) -> Result<Vec<i64>> {
        let mut v: Vec<i64> =
            results(self.get("stock/", &[("part", part.to_string())]).await?).iter().filter_map(|s| s["location"].as_i64()).collect();
        v.dedup();
        Ok(v)
    }

    /// Active parts matching a free-text search.
    pub async fn search_parts(&self, q: &str) -> Result<Vec<PartLite>> {
        let q = [("search", q.to_string()), ("active", "true".into()), ("limit", "8".into())];
        Ok(results(self.get("part/", &q).await?)
            .iter()
            .filter_map(|p| {
                Some(PartLite {
                    pk: p["pk"].as_i64()?,
                    name: p["name"].as_str()?.to_string(),
                    description: p["description"].as_str().unwrap_or_default().to_string(),
                })
            })
            .collect())
    }

    /// Adds a photo to a part as an attachment (its main image stays as it is).
    pub async fn attach_photo(&self, part: i64, photo: Vec<u8>, file_name: &str, comment: &str) -> Result<()> {
        let file = || multipart::Part::bytes(photo.clone()).file_name(file_name.to_string()).mime_str("image/jpeg");
        // InvenTree >= 0.16: generic attachments; older versions: per-model endpoint
        let form = multipart::Form::new()
            .text("model_type", "part")
            .text("model_id", part.to_string())
            .text("comment", comment.to_string())
            .part("attachment", file()?);
        if let Err(e) = self.send(self.http.post(self.url("attachment/", &[])?).multipart(form)).await {
            tracing::debug!("generic attachment failed ({e:#}), trying part/attachment/");
            let form =
                multipart::Form::new().text("part", part.to_string()).text("comment", comment.to_string()).part("attachment", file()?);
            self.send(self.http.post(self.url("part/attachment/", &[])?).multipart(form)).await?;
        }
        Ok(())
    }

    /// Adds `qty` of `part` at `location`: to the stock item already there when `merge`, else as a new item.
    /// Returns the stock item pk.
    pub async fn add_stock(&self, part: i64, location: i64, qty: f64, merge: bool, note: &str) -> Result<i64> {
        if merge && let Some(existing) = self.stock_at(part, location).await?.first().copied() {
            self.post("stock/add/", &json!({"items": [{"pk": existing, "quantity": qty}], "notes": note})).await?;
            return Ok(existing);
        }
        stock_pk(self.post("stock/", &json!({"part": part, "location": location, "quantity": qty, "notes": note})).await?)
    }

    /// Deletes a quick-capture part and its stock (InvenTree only deletes inactive parts). Already gone is fine.
    pub async fn delete_part(&self, part: i64) -> Result<()> {
        if let Err(e) = self.get(&format!("part/{part}/"), &[]).await {
            if format!("{e:#}").contains("404") {
                return Ok(());
            }
            return Err(e);
        }
        for s in results(self.get("stock/", &[("part", part.to_string())]).await?) {
            self.delete(&format!("stock/{}/", pk(&s)?)).await?;
        }
        self.patch(&format!("part/{part}/"), &json!({"active": false})).await?;
        self.delete(&format!("part/{part}/")).await?;
        Ok(())
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
        assert_eq!(stock_pk(json!({"pk": 3})).unwrap(), 3);
        assert_eq!(stock_pk(json!([{"pk": 4}])).unwrap(), 4);
    }
}
