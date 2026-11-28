//! The background worker: moves each capture through `queued → stored → (research →) named`.
//! "Don't know" (`Sort`) captures then get box suggestions.
//!
//! One unit of work at a time, and storing always beats naming, so a burst of photos lands in
//! InvenTree within seconds and identification catches up afterwards.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use tokio::sync::Notify;

use crate::{
    config::ResearchPolicy,
    dedup,
    inventree::{self, Inv},
    research::{self, Searx},
    spool::{self, Job, Mode, Spool, Status},
    suggest,
    vision::{self, Guess, Vision},
};

const MAX_ATTEMPTS: u32 = 3;

#[derive(Clone)]
pub struct Pipeline {
    pub inv: Arc<Inv>,
    pub vision: Arc<Vision>,
    pub searx: Option<Arc<Searx>>,
    pub spool: Arc<Spool>,
    pub research: ResearchPolicy,
    pub dedup: bool,
    pub wake: Arc<Notify>,
}

impl Pipeline {
    pub async fn run(self) {
        loop {
            match self.step().await {
                Ok(true) => {}
                Ok(false) => {
                    match self.spool.prune(spool::KEEP) {
                        Ok(0) => {}
                        Ok(n) => tracing::info!("pruned {n} old log entries"),
                        Err(e) => tracing::warn!("prune: {e:#}"),
                    }
                    tokio::select! {
                        _ = self.wake.notified() => {}
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                    }
                }
                Err(e) => {
                    tracing::warn!("worker: {e:#}");
                    tokio::time::sleep(Duration::from_secs(15)).await;
                }
            }
        }
    }

    /// Does one unit of work; `Ok(false)` when there is nothing to do.
    async fn step(&self) -> Result<bool> {
        let jobs = self.spool.load_all()?;
        let next = |s: Status| jobs.iter().find(|j| j.status == s).cloned();

        if let Some(mut j) = next(Status::Queued) {
            let res = self.store(&mut j).await;
            if let Err(e) = &res {
                j.fail(format!("{e:#}"));
            }
            self.spool.save_from_worker(&j)?;
            res?;
            return Ok(true);
        }

        if let Some(mut j) = next(Status::Stored) {
            match self.identify(&mut j).await {
                Ok(()) => {
                    // the photos stay until the duplicate check (they may become attachments)
                    j.error = None;
                    j.attempts = 0;
                }
                Err(e) => {
                    tracing::warn!("identify {}: {e:#}", j.id);
                    j.fail(format!("{e:#}"));
                    if j.attempts >= MAX_ATTEMPTS {
                        j.status = Status::Unnamed;
                        self.spool.drop_photos(&j);
                    } else {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
            self.spool.save_from_worker(&j)?;
            return Ok(true);
        }

        if let Some(mut j) = next(Status::Research) {
            if let Err(e) = self.research(&mut j).await {
                tracing::warn!("research {}: {e:#}", j.id);
                j.fail(format!("research: {e:#}"));
                if j.attempts < MAX_ATTEMPTS {
                    self.spool.save_from_worker(&j)?;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    return Ok(true);
                }
            }
            // research failing for good keeps the first guess
            j.status = Status::Named;
            self.spool.save_from_worker(&j)?;
            return Ok(true);
        }

        if let Some(mut j) = jobs.iter().find(|j| j.status == Status::Named && !j.dup_checked).cloned() {
            let res = if self.dedup { self.check_duplicate(&mut j).await } else { Ok(()) };
            let done = match res {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!("duplicate check {}: {e:#}", j.id);
                    j.fail(format!("duplicate check: {e:#}"));
                    // a merge that has started (merged_into set) must finish, so it keeps retrying
                    j.attempts >= MAX_ATTEMPTS && j.merged_into.is_none()
                }
            };
            if done {
                j.dup_checked = true;
                j.attempts = 0;
                j.error = None;
                self.spool.drop_photos(&j);
            }
            self.spool.save_from_worker(&j)?;
            if !done {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            return Ok(true);
        }

        let ready = |j: &Job| j.status == Status::Unnamed || (j.status == Status::Named && j.dup_checked);
        if let Some(mut j) = jobs.iter().find(|j| j.in_inbox() && ready(j) && j.suggestions.is_none()).cloned() {
            match self.suggest(&j).await {
                Ok(s) => {
                    j.suggestions = Some(s);
                    j.attempts = 0;
                    j.error = None;
                }
                Err(e) => {
                    tracing::warn!("suggest {}: {e:#}", j.id);
                    j.fail(format!("suggest: {e:#}"));
                    if j.attempts >= MAX_ATTEMPTS {
                        j.suggestions = Some(vec![]);
                    }
                }
            }
            self.spool.save_from_worker(&j)?;
            if j.suggestions.is_none() {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            return Ok(true);
        }

        Ok(false)
    }

    /// queued → stored: an Inbox part with the first photo as its image, any further photos as attachments,
    /// and stock at the box (`Sort` captures: at the Inbox location).
    async fn store(&self, j: &mut Job) -> Result<()> {
        let paths = self.spool.photo_paths(j);
        let photo = std::fs::read(&paths[0])?;
        if j.mode == Mode::Sort {
            j.location = Some(self.inv.ensure_location(inventree::INBOX).await?);
            j.location_name = inventree::INBOX.to_string();
        }
        let location = j.location.context("job has no location")?;
        let local = j.created.with_timezone(&chrono::Local);
        let name = match j.mode {
            Mode::Store => format!("Inbox {} {}", j.location_name, local.format("%d %b %H:%M:%S")),
            Mode::Sort => format!("Inbox (to store) {}", local.format("%d %b %H:%M:%S")),
        };
        let ipn = format!("qc-{}", &j.id[..10]);
        let (pk, stock) = self.inv.create_inbox_part(&ipn, &name, location, j.qty, photo).await?;
        j.part = Some(pk);
        j.stock = Some(stock);
        // saved after each one, so a retry carries on where it stopped instead of attaching twice
        while (j.attached as usize) + 1 < paths.len() {
            let n = j.attached as usize + 1;
            let bytes = std::fs::read(&paths[n])?;
            let file = format!("{ipn}-{}.jpg", n + 1);
            self.inv.attach_photo(pk, bytes, &file, &format!("Quick capture photo {} of {}", n + 1, paths.len())).await?;
            j.attached += 1;
            self.spool.save_from_worker(j)?;
        }
        j.name = Some(name);
        j.status = Status::Stored;
        j.attempts = 0;
        j.error = None;
        tracing::info!("stored {} as part {pk}", j.id);
        Ok(())
    }

    /// stored → named | research: first look by the vision model.
    async fn identify(&self, j: &mut Job) -> Result<()> {
        let imgs = vision::model_images(&self.spool.photo_paths(j))?;
        let g = self.vision.ask(&vision::identify_prompt(imgs.len()), &imgs).await?;
        self.apply(j, &g, "").await?;
        j.markings = Some(g.markings.trim().to_string()).filter(|m| !m.is_empty());

        let wanted = match self.research {
            ResearchPolicy::Markings => true,
            ResearchPolicy::Low => g.confidence() == "low",
            ResearchPolicy::Off => false,
        };
        let searchable = !research::queries(j.markings.as_deref().unwrap_or_default()).is_empty();
        j.status = if wanted && self.searx.is_some() && searchable { Status::Research } else { Status::Named };
        Ok(())
    }

    /// research → named: search the markings, then ask the model again with the photo + results.
    async fn research(&self, j: &mut Job) -> Result<()> {
        let searx = self.searx.as_ref().context("SEARXNG_URL not configured")?;
        let markings = j.markings.clone().unwrap_or_default();
        let hits = searx.search(&research::queries(&markings)).await?;
        if hits.is_empty() {
            tracing::info!("research {}: no search results", j.id);
            j.status = Status::Named;
            return Ok(());
        }
        let imgs = vision::model_images(&self.spool.photo_paths(j))?;
        let prompt =
            vision::research_prompt(imgs.len(), &markings, j.name.as_deref().unwrap_or_default(), &research::format_for_prompt(&hits));
        let g = self.vision.ask(&prompt, &imgs).await?;
        let notes = format!("\n\n**Web research** (SearXNG, {} results):\n{}", hits.len(), research::format_sources(&hits));
        self.apply(j, &g, &notes).await?;
        j.sources = hits.into_iter().map(|h| h.url).collect();
        j.status = Status::Named;
        Ok(())
    }

    /// Boxes for a `Sort` capture, from what the other boxes already hold.
    async fn suggest(&self, j: &Job) -> Result<Vec<spool::Suggestion>> {
        let Some(name) = j.name.clone().filter(|_| j.status == Status::Named) else {
            return Ok(vec![]); // nothing to go on
        };
        let part = j.part.context("job has no part")?;
        let (category, description) = self.inv.part_info(part).await?;
        let inbox = self.inv.ensure_location(inventree::INBOX).await?;
        let inbox_cat = self.inv.ensure_category(inventree::INBOX).await?;
        let boxes = self.inv.box_contents(inbox).await?;
        let list = suggest::shortlist(&boxes, &name, category.filter(|c| *c != inbox_cat));
        if list.is_empty() && j.merged_into.is_none() {
            return Ok(vec![]);
        }
        let reply = if list.is_empty() {
            None
        } else {
            match self.vision.chat(&suggest::prompt(&name, &description, &list), &[]).await {
                Ok(r) => Some(r),
                Err(e) => {
                    tracing::warn!("suggest {}: model failed, using ranking only: {e:#}", j.id);
                    None
                }
            }
        };
        let mut s = suggest::choose(reply.as_deref(), &list);
        // a part already in InvenTree: the boxes that hold it come first
        if j.merged_into.is_some() {
            let mut have = Vec::new();
            for l in self.inv.part_locations(part).await?.into_iter().filter(|l| *l != inbox) {
                if let Some(b) = boxes.iter().find(|b| b.pk == l) {
                    have.push(spool::Suggestion { location: l, name: b.name.clone(), reason: "already holds this part".into() });
                }
            }
            s.retain(|x| !have.iter().any(|h| h.location == x.location));
            have.append(&mut s);
            have.truncate(suggest::MAX);
            s = have;
        }
        tracing::info!("suggested for {}: {:?}", j.id, s.iter().map(|x| &x.name).collect::<Vec<_>>());
        Ok(s)
    }

    /// Is this capture a part already in InvenTree? Then its photo becomes an attachment of that part, its
    /// qty is added to that part's stock in the box, and the new part is deleted.
    ///
    /// Crash-safe: `merged_into` is saved as soon as the stock has been added, so a retry only finishes
    /// removing the new part and never adds the stock twice.
    async fn check_duplicate(&self, j: &mut Job) -> Result<()> {
        let new_part = j.part.context("job has no part")?;
        if j.merged_into.is_none() {
            let Some(existing) = self.find_existing(j, new_part).await? else { return Ok(()) };
            let location = j.location.context("job has no location")?;
            let when = j.created.with_timezone(&chrono::Local).format("%d %b %H:%M");
            let note = format!("Quick capture {when}: +{} (inventory-assistant)", j.qty);
            for (n, path) in self.spool.photo_paths(j).iter().enumerate() {
                if let Ok(photo) = std::fs::read(path) {
                    self.inv.attach_photo(existing.pk, photo, &format!("capture-{}-{}.jpg", &j.id[..10], n + 1), &note).await?;
                }
            }
            // a "Don't know" capture gets its own stock item in the Inbox, so it can be filed on its own
            let stock = self.inv.add_stock(existing.pk, location, j.qty, j.mode == Mode::Store, &note).await?;
            j.merged_into = Some(existing.pk);
            j.stock = Some(stock);
            j.name = Some(existing.name.chars().take(100).collect());
            self.spool.save_from_worker(j)?;
            tracing::info!("{}: same as part {} ({}), stock +{}", j.id, existing.pk, existing.name, j.qty);
        }
        let existing = j.merged_into.context("unreachable")?;
        if new_part != existing {
            self.inv.delete_part(new_part).await?;
            j.part = Some(existing);
        }
        Ok(())
    }

    /// The part already in InvenTree that this capture shows, if any.
    async fn find_existing(&self, j: &Job, new_part: i64) -> Result<Option<inventree::PartLite>> {
        let name = j.name.clone().unwrap_or_default();
        let markings = j.markings.clone().unwrap_or_default();
        let mut queries = vec![name.clone()];
        queries.extend(research::part_numbers(&markings).into_iter().take(2));
        let mut cands: Vec<inventree::PartLite> = Vec::new();
        for q in queries.iter().filter(|q| !q.trim().is_empty()) {
            for p in self.inv.search_parts(q).await? {
                // skip this capture itself and other captures still awaiting identification
                if p.pk != new_part && !p.name.starts_with("Inbox ") && !cands.iter().any(|c| c.pk == p.pk) {
                    cands.push(p);
                }
            }
        }
        cands.truncate(8);
        if cands.is_empty() {
            return Ok(None);
        }
        if let Some(i) = dedup::exact(&name, &cands) {
            return Ok(Some(cands.swap_remove(i)));
        }
        let (_, description) = self.inv.part_info(new_part).await?;
        let reply = self.vision.chat(&dedup::prompt(&name, &description, &markings, &cands), &[]).await?;
        Ok(dedup::parse(&reply, cands.len()).map(|i| cands.swap_remove(i)))
    }

    /// Writes a guess to the InvenTree part. Low-confidence items keep their Inbox category.
    async fn apply(&self, j: &mut Job, g: &Guess, extra_notes: &str) -> Result<()> {
        let pk = j.part.context("job has no part")?;
        let name = g.name.trim();
        let conf = g.confidence();
        let category = if conf == "low" { None } else { inventree::category_for(&g.category) };
        let place = match j.mode {
            Mode::Store => format!("into box `{}`", j.location_name),
            Mode::Sort => "into the Inbox (box not chosen yet)".to_string(),
        };
        let note = format!(
            "**Quick capture** {place}. AI guess ({}, confidence **{conf}**):\n\n{}\n\nMarkings read: {}{extra_notes}",
            self.vision.model, g.description, g.markings
        );
        self.inv.apply_guess(pk, name, &g.description, &note, category).await?;
        j.name = Some(name.chars().take(100).collect());
        j.confidence = Some(conf.to_string());
        tracing::info!("named part {pk}: {name} ({conf})");
        Ok(())
    }
}
