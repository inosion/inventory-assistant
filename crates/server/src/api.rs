//! HTTP API used by the capture UI. Authentication is delegated to the reverse proxy
//! (InvenTree's `/auth/` endpoint via `forward_auth`), see `deploy/Caddyfile`.

use std::sync::Arc;

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::{
    inventree::{self, Inv},
    spool::{self, Job, Mode, Spool},
    vision,
};

#[derive(Clone)]
pub struct ApiState {
    pub inv: Arc<Inv>,
    pub spool: Arc<Spool>,
    pub wake: Arc<Notify>,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/locations", get(get_locations).post(post_location))
        .route("/api/capture", post(capture))
        .route("/api/recent", get(recent))
        .route("/api/thumb/{id}", get(thumb))
        .route("/api/jobs/{id}/file", post(file_job))
        .layer(DefaultBodyLimit::max(40 * 1024 * 1024))
        .with_state(state)
}

struct ApiError(StatusCode, anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        tracing::warn!("api error: {:#}", self.1);
        (self.0, format!("{:#}", self.1)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(StatusCode::BAD_GATEWAY, e.into())
    }
}

fn client_err(code: StatusCode, msg: &str) -> ApiError {
    ApiError(code, anyhow::anyhow!("{msg}"))
}

type ApiResult<T> = Result<T, ApiError>;

/// All stock locations ("boxes"), sorted by path. The Inbox location is internal and left out.
async fn get_locations(State(s): State<ApiState>) -> ApiResult<Json<Value>> {
    let mut locs = s.inv.list_locations().await?;
    locs.retain(|l| l.name != inventree::INBOX);
    Ok(Json(json!(locs)))
}

#[derive(Deserialize)]
struct NewLoc {
    name: String,
}

/// Finds or creates a location; `B1/cables` creates the nested path.
async fn post_location(State(s): State<ApiState>, Json(body): Json<NewLoc>) -> ApiResult<Json<Value>> {
    let pk = s.inv.ensure_location(&body.name).await?;
    let path = s.inv.location_path(pk).await?;
    Ok(Json(json!({"pk": pk, "name": path, "items": 0})))
}

/// Multipart: `photo` (JPEG), `qty`, `mode` (`store` | `sort`), and for `store` also `location` (pk) and
/// `location_name`. Spools and returns immediately.
async fn capture(State(s): State<ApiState>, mut mp: Multipart) -> ApiResult<Json<Job>> {
    let (mut photo, mut location, mut location_name, mut qty, mut mode) = (None, None, String::new(), 1.0, Mode::Store);
    while let Some(field) = mp.next_field().await? {
        match field.name().unwrap_or_default() {
            "photo" => photo = Some(field.bytes().await?),
            "mode" => {
                if field.text().await?.trim().eq_ignore_ascii_case("sort") {
                    mode = Mode::Sort
                }
            }
            "location" => location = Some(field.text().await?.trim().parse::<i64>().context("bad location")?),
            "location_name" => location_name = field.text().await?,
            "qty" => qty = field.text().await?.trim().parse::<f64>().unwrap_or(1.0).max(1.0),
            _ => {}
        }
    }
    let photo = photo.context("missing photo")?;
    let job = match mode {
        Mode::Store => Job::new(mode, Some(location.context("missing location")?), location_name, qty),
        // the worker puts it in the Inbox location
        Mode::Sort => Job::new(mode, None, inventree::INBOX.to_string(), qty),
    };
    s.spool.put_photo(&job, &photo)?;
    let thumb_path = s.spool.thumb_path(&job.id);
    let bytes = photo.clone();
    let thumb = tokio::task::spawn_blocking(move || vision::thumbnail(&bytes).and_then(|t| Ok(std::fs::write(thumb_path, t)?)));
    if let Ok(Err(e)) = thumb.await {
        tracing::warn!("thumbnail {}: {e:#}", job.id); // not fatal: the log just shows no picture
    }
    s.spool.save(&job)?;
    s.wake.notify_one();
    tracing::info!("captured {} -> {} ({} bytes)", job.id, job.location_name, photo.len());
    Ok(Json(job))
}

/// The capture logs, newest first (the UI polls this for progress): the last 100 Store captures, every
/// Sort capture still in the Inbox, and the last 100 filed ones.
async fn recent(State(s): State<ApiState>) -> ApiResult<Json<Vec<Job>>> {
    Ok(Json(logs(s.spool.load_all()?, spool::KEEP)))
}

fn logs(mut jobs: Vec<Job>, keep: usize) -> Vec<Job> {
    jobs.reverse();
    let mut stores = 0;
    let mut filed_jobs: Vec<&Job> = jobs.iter().filter(|j| j.mode == Mode::Sort && j.filed.is_some()).collect();
    filed_jobs.sort_by_key(|j| std::cmp::Reverse(j.filed));
    let keep_filed: Vec<String> = filed_jobs.into_iter().take(keep).map(|j| j.id.clone()).collect();
    jobs.retain(|j| match (j.mode, j.filed.is_some()) {
        (Mode::Store, _) => {
            stores += 1;
            stores <= keep
        }
        (Mode::Sort, false) => true,
        (Mode::Sort, true) => keep_filed.contains(&j.id),
    });
    jobs
}

async fn thumb(State(s): State<ApiState>, Path(id): Path<String>) -> Response {
    if !spool::valid_id(&id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match tokio::fs::read(s.spool.thumb_path(&id)).await {
        Ok(b) => ([(header::CONTENT_TYPE, "image/jpeg"), (header::CACHE_CONTROL, "private, max-age=86400")], b).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
struct FileTo {
    location: i64,
    location_name: String,
}

/// Moves a "Don't know" capture from the Inbox into the box the user chose.
async fn file_job(State(s): State<ApiState>, Path(id): Path<String>, Json(to): Json<FileTo>) -> ApiResult<Json<Job>> {
    let job = s.spool.load(&id).map_err(|_| client_err(StatusCode::NOT_FOUND, "no such capture"))?;
    if job.mode != Mode::Sort {
        return Err(client_err(StatusCode::CONFLICT, "only Don't-know captures can be filed"));
    }
    let (Some(part), Some(stock)) = (job.part, job.stock) else {
        return Err(client_err(StatusCode::CONFLICT, "not in InvenTree yet, try again in a moment"));
    };
    let note = format!("Filed from the Inbox into {} (inventory-assistant)", to.location_name);
    s.inv.move_stock(stock, part, to.location, &note).await?;
    let (job, ()) = s.spool.update(&id, |j| {
        j.location = Some(to.location);
        j.location_name = to.location_name.clone();
        j.filed = Some(chrono::Utc::now());
        Ok(())
    })?;
    tracing::info!("filed {} -> {}", job.id, job.location_name);
    Ok(Json(job))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_cap_each_list_but_keep_the_inbox() {
        let t0 = chrono::Utc::now();
        let mut jobs = Vec::new();
        for i in 0..5 {
            let mut j = Job::new(Mode::Store, Some(1), "A1".into(), 1.0);
            j.created = t0 + chrono::Duration::seconds(i);
            jobs.push(j);
        }
        for i in 0..4 {
            let mut j = Job::new(Mode::Sort, Some(2), "Inbox".into(), 1.0);
            j.created = t0 + chrono::Duration::seconds(10 + i);
            j.filed = (i % 2 == 0).then(|| t0 + chrono::Duration::seconds(100 - i));
            jobs.push(j);
        }
        let l = logs(jobs.clone(), 1);
        assert_eq!(l.iter().filter(|j| j.mode == Mode::Store).count(), 1);
        assert_eq!(l.iter().find(|j| j.mode == Mode::Store).unwrap().id, jobs[4].id);
        assert_eq!(l.iter().filter(|j| j.in_inbox()).count(), 2);
        let filed: Vec<_> = l.iter().filter(|j| j.filed.is_some()).collect();
        assert_eq!(filed.len(), 1);
        assert_eq!(filed[0].id, jobs[5].id, "most recently filed wins");
    }
}
