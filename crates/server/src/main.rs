//! inventory-assistant: AI-assisted, photo-first capture into InvenTree.
//!
//! The phone uploads a (downscaled) photo + box + qty. It is spooled to disk and the request
//! returns at once. A single background worker then moves each capture along:
//!
//! 1. `queued -> stored`: creates an "Inbox" part (photo as image) with stock at the box.
//! 2. `stored -> named`: asks a local vision model (Ollama) to name and categorise it
//!    (after 3 failed tries it is `unnamed` and simply stays in Inbox).
//! 3. `research -> named`: items with part-number-like markings are web-searched (self-hosted
//!    SearXNG) and the model is re-asked with the photo + results.
//!
//! InvenTree is the source of truth; the spool is only a durable queue + recent-activity log.

mod api;
mod config;
mod inventree;
mod pipeline;
mod research;
mod spool;
mod vision;

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use tokio::sync::Notify;
use tower_http::services::{ServeDir, ServeFile};

use crate::{config::Config, inventree::Inv, pipeline::Pipeline, research::Searx, spool::Spool, vision::Vision};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cfg = Config::from_env()?;
    let http = reqwest::Client::builder().timeout(Duration::from_secs(120)).build()?;
    let inv = Arc::new(Inv::new(http.clone(), cfg.inventree_url.clone(), cfg.inventree_token.clone()));
    let spool = Arc::new(Spool::open(&cfg.data_dir)?);
    let wake = Arc::new(Notify::new());

    let pipeline = Pipeline {
        inv: inv.clone(),
        vision: Arc::new(Vision::new(http.clone(), cfg.ollama_url.clone(), cfg.model.clone())),
        searx: cfg.searxng_url.clone().map(|u| Arc::new(Searx::new(http.clone(), u))),
        spool: spool.clone(),
        research: cfg.research,
        wake: wake.clone(),
    };
    tracing::info!(
        "model {} at {}; research {:?} via {}",
        cfg.model,
        cfg.ollama_url,
        cfg.research,
        cfg.searxng_url.as_deref().unwrap_or("(disabled)")
    );
    tokio::spawn(pipeline.run());

    let ui = ServeDir::new(&cfg.dist_dir).fallback(ServeFile::new(cfg.dist_dir.join("index.html")));
    let app = api::router(api::ApiState { inv, spool, wake }).fallback_service(ui);

    tracing::info!("listening on {}", cfg.listen);
    axum::serve(tokio::net::TcpListener::bind(cfg.listen).await?, app).await?;
    Ok(())
}
