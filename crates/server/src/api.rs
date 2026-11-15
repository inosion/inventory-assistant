//! HTTP API used by the capture UI. Authentication is delegated to the reverse proxy
//! (InvenTree's `/auth/` endpoint via `forward_auth`), see `deploy/Caddyfile`.

use std::sync::Arc;

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::{
    inventree::Inv,
    spool::{Job, Spool},
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
        .layer(DefaultBodyLimit::max(40 * 1024 * 1024))
        .with_state(state)
}

struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        tracing::warn!("api error: {:#}", self.0);
        (StatusCode::BAD_GATEWAY, format!("{:#}", self.0)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// All stock locations ("boxes"), sorted by path.
async fn get_locations(State(s): State<ApiState>) -> ApiResult<Json<Value>> {
    Ok(Json(json!(s.inv.list_locations().await?)))
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

/// Multipart: `photo` (JPEG), `location` (pk), `location_name`, `qty`. Spools and returns immediately.
async fn capture(State(s): State<ApiState>, mut mp: Multipart) -> ApiResult<Json<Job>> {
    let (mut photo, mut location, mut location_name, mut qty) = (None, None, String::new(), 1.0);
    while let Some(field) = mp.next_field().await? {
        match field.name().unwrap_or_default() {
            "photo" => photo = Some(field.bytes().await?),
            "location" => location = Some(field.text().await?.trim().parse::<i64>().context("bad location")?),
            "location_name" => location_name = field.text().await?,
            "qty" => qty = field.text().await?.trim().parse::<f64>().unwrap_or(1.0).max(1.0),
            _ => {}
        }
    }
    let photo = photo.context("missing photo")?;
    let job = Job::new(location.context("missing location")?, location_name, qty);
    s.spool.put_photo(&job, &photo)?;
    s.spool.save(&job)?;
    s.wake.notify_one();
    tracing::info!("captured {} -> {} ({} bytes)", job.id, job.location_name, photo.len());
    Ok(Json(job))
}

/// The 80 most recent captures, newest first (the UI polls this for progress).
async fn recent(State(s): State<ApiState>) -> ApiResult<Json<Vec<Job>>> {
    let mut jobs = s.spool.load_all()?;
    jobs.reverse();
    jobs.truncate(80);
    Ok(Json(jobs))
}
