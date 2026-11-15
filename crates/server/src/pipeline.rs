//! The background worker: moves each capture through `queued → stored → (research →) named`.
//!
//! One unit of work at a time, and storing always beats naming, so a burst of photos lands in
//! InvenTree within seconds and identification catches up afterwards.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use tokio::sync::Notify;

use crate::{
    config::ResearchPolicy,
    inventree::{self, Inv},
    research::{self, Searx},
    spool::{Job, Spool, Status},
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
    pub wake: Arc<Notify>,
}

impl Pipeline {
    pub async fn run(self) {
        loop {
            match self.step().await {
                Ok(true) => {}
                Ok(false) => {
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
            self.spool.save(&j)?;
            res?;
            return Ok(true);
        }

        if let Some(mut j) = next(Status::Stored) {
            match self.identify(&mut j).await {
                Ok(()) => {
                    j.error = None;
                    j.attempts = 0;
                    if j.status == Status::Named {
                        self.spool.drop_photo(&j);
                    }
                }
                Err(e) => {
                    tracing::warn!("identify {}: {e:#}", j.id);
                    j.fail(format!("{e:#}"));
                    if j.attempts >= MAX_ATTEMPTS {
                        j.status = Status::Unnamed;
                        self.spool.drop_photo(&j);
                    } else {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
            self.spool.save(&j)?;
            return Ok(true);
        }

        if let Some(mut j) = next(Status::Research) {
            if let Err(e) = self.research(&mut j).await {
                tracing::warn!("research {}: {e:#}", j.id);
                j.fail(format!("research: {e:#}"));
                if j.attempts < MAX_ATTEMPTS {
                    self.spool.save(&j)?;
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    return Ok(true);
                }
            }
            // research failing for good keeps the first guess
            j.status = Status::Named;
            self.spool.drop_photo(&j);
            self.spool.save(&j)?;
            return Ok(true);
        }

        Ok(false)
    }

    /// queued → stored: an Inbox part with the photo as its image, and stock at the box.
    async fn store(&self, j: &mut Job) -> Result<()> {
        let photo = std::fs::read(self.spool.photo_path(j))?;
        let local = j.created.with_timezone(&chrono::Local);
        let name = format!("Inbox {} {}", j.location_name, local.format("%d %b %H:%M:%S"));
        let ipn = format!("qc-{}", &j.id[..10]);
        let pk = self.inv.create_inbox_part(&ipn, &name, j.location, j.qty, photo).await?;
        j.part = Some(pk);
        j.name = Some(name);
        j.status = Status::Stored;
        j.attempts = 0;
        j.error = None;
        tracing::info!("stored {} as part {pk}", j.id);
        Ok(())
    }

    /// stored → named | research: first look by the vision model.
    async fn identify(&self, j: &mut Job) -> Result<()> {
        let img = vision::model_image(&self.spool.photo_path(j))?;
        let g = self.vision.ask(&vision::identify_prompt(), &img).await?;
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
        let prompt = vision::research_prompt(&markings, j.name.as_deref().unwrap_or_default(), &research::format_for_prompt(&hits));
        let img = vision::model_image(&self.spool.photo_path(j))?;
        let g = self.vision.ask(&prompt, &img).await?;
        let notes = format!("\n\n**Web research** (SearXNG, {} results):\n{}", hits.len(), research::format_sources(&hits));
        self.apply(j, &g, &notes).await?;
        j.sources = hits.into_iter().map(|h| h.url).collect();
        j.status = Status::Named;
        Ok(())
    }

    /// Writes a guess to the InvenTree part. Low-confidence items keep their Inbox category.
    async fn apply(&self, j: &mut Job, g: &Guess, extra_notes: &str) -> Result<()> {
        let pk = j.part.context("job has no part")?;
        let name = g.name.trim();
        let conf = g.confidence();
        let category = if conf == "low" { None } else { inventree::category_for(&g.category) };
        let note = format!(
            "**Quick capture** into box `{}`. AI guess ({}, confidence **{conf}**):\n\n{}\n\nMarkings read: {}{extra_notes}",
            j.location_name, self.vision.model, g.description, g.markings
        );
        self.inv.apply_guess(pk, name, &g.description, &note, category).await?;
        j.name = Some(name.chars().take(100).collect());
        j.confidence = Some(conf.to_string());
        tracing::info!("named part {pk}: {name} ({conf})");
        Ok(())
    }
}
