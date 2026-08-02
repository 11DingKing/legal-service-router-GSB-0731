//! HTTP API (Axum). Handlers are thin wrappers over [`Store`] and [`routing`].
//!
//! Endpoints:
//! - `POST /catalog/import`            import a catalog version (hot reload)
//! - `POST /catalog/activate`          switch the active version
//! - `GET  /catalog/versions`          list versions + active pointer
//! - `POST /route`                     route + persist an immutable snapshot
//! - `POST /route/batch`               batch routing over one captured version
//! - `POST /closures/start`            begin a temporary closure
//! - `POST /closures/end`              end a temporary closure
//! - `POST /degradations/start`        begin a temporary capability degradation
//! - `POST /degradations/end`          end a temporary capability degradation
//! - `GET  /snapshots/:id`             replay a stored snapshot verbatim

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::routing::{self, RouteRequest, RouteResult};
use crate::store::Store;

pub type AppState = Arc<Store>;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/catalog/import", post(import_catalog))
        .route("/catalog/activate", post(activate_catalog))
        .route("/catalog/versions", get(list_versions))
        .route("/route", post(route_once))
        .route("/route/batch", post(route_batch))
        .route("/closures/start", post(start_closure))
        .route("/closures/end", post(end_closure))
        .route("/degradations/start", post(start_degradation))
        .route("/degradations/end", post(end_degradation))
        .route("/snapshots/:id", get(get_snapshot))
        .with_state(state)
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ErrorBody>) {
    (status, Json(ErrorBody { error: msg.into() }))
}

#[derive(Deserialize)]
struct ImportQuery {
    /// Raw catalog JSON (same shape as `service-catalog.json`).
    catalog: serde_json::Value,
    /// Activate this version immediately. Defaults to true.
    #[serde(default = "default_true")]
    activate: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize)]
struct ImportResponse {
    catalog_version: String,
    activated: bool,
}

async fn import_catalog(
    State(store): State<AppState>,
    Json(body): Json<ImportQuery>,
) -> impl IntoResponse {
    let bytes = match serde_json::to_vec(&body.catalog) {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    match store.import_catalog(&bytes, body.activate) {
        Ok(version) => (
            StatusCode::CREATED,
            Json(ImportResponse { catalog_version: version, activated: body.activate }),
        )
            .into_response(),
        Err(e) => err(StatusCode::CONFLICT, e).into_response(),
    }
}

#[derive(Deserialize)]
struct ActivateQuery {
    version: String,
}

async fn activate_catalog(
    State(store): State<AppState>,
    Json(body): Json<ActivateQuery>,
) -> impl IntoResponse {
    match store.activate(&body.version) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err(StatusCode::NOT_FOUND, e).into_response(),
    }
}

#[derive(Serialize)]
struct VersionsResponse {
    versions: Vec<String>,
    active: Option<String>,
}

async fn list_versions(State(store): State<AppState>) -> impl IntoResponse {
    let (versions, active) = store.list_versions();
    Json(VersionsResponse { versions, active })
}

#[derive(Serialize)]
struct RouteResponse {
    snapshot_id: String,
    result: RouteResult,
}

async fn route_once(
    State(store): State<AppState>,
    Json(req): Json<RouteRequest>,
) -> impl IntoResponse {
    // Capture exactly one complete catalog version for this request.
    let catalog = match store.active_catalog() {
        Some(c) => c,
        None => return err(StatusCode::CONFLICT, "no active catalog").into_response(),
    };
    let result = routing::route(&catalog, &req);
    match store.save_snapshot(&req, &result) {
        Ok(snapshot_id) => Json(RouteResponse { snapshot_id, result }).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct BatchQuery {
    requests: Vec<RouteRequest>,
    /// Persist each result as a snapshot. Defaults to true.
    #[serde(default = "default_true")]
    persist: bool,
}

#[derive(Serialize)]
struct BatchItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_id: Option<String>,
    result: RouteResult,
}

#[derive(Serialize)]
struct BatchResponse {
    catalog_version: String,
    items: Vec<BatchItem>,
}

async fn route_batch(
    State(store): State<AppState>,
    Json(body): Json<BatchQuery>,
) -> impl IntoResponse {
    // A batch captures a single catalog version so every item in the batch is
    // evaluated against the same complete snapshot, even under hot reload.
    let catalog = match store.active_catalog() {
        Some(c) => c,
        None => return err(StatusCode::CONFLICT, "no active catalog").into_response(),
    };
    let mut items = Vec::with_capacity(body.requests.len());
    for req in &body.requests {
        let result = routing::route(&catalog, req);
        let snapshot_id = if body.persist {
            match store.save_snapshot(req, &result) {
                Ok(id) => Some(id),
                Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            }
        } else {
            None
        };
        items.push(BatchItem { snapshot_id, result });
    }
    Json(BatchResponse { catalog_version: catalog.version.clone(), items }).into_response()
}

#[derive(Deserialize)]
struct StartClosureQuery {
    version: String,
    event_id: String,
    point_id: String,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
}

async fn start_closure(
    State(store): State<AppState>,
    Json(body): Json<StartClosureQuery>,
) -> impl IntoResponse {
    match store.start_closure(&body.version, &body.event_id, &body.point_id, body.from, body.to) {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

#[derive(Deserialize)]
struct EndClosureQuery {
    version: String,
    event_id: String,
    #[serde(default)]
    at: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
struct EndClosureResponse {
    ended_at: DateTime<Utc>,
}

async fn end_closure(
    State(store): State<AppState>,
    Json(body): Json<EndClosureQuery>,
) -> impl IntoResponse {
    match store.end_closure(&body.version, &body.event_id, body.at) {
        Ok(ended_at) => Json(EndClosureResponse { ended_at }).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

#[derive(Deserialize)]
struct StartDegradationQuery {
    version: String,
    event_id: String,
    point_id: String,
    /// Access capability temporarily removed while active.
    capability: String,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
}

async fn start_degradation(
    State(store): State<AppState>,
    Json(body): Json<StartDegradationQuery>,
) -> impl IntoResponse {
    match store.start_degradation(
        &body.version,
        &body.event_id,
        &body.point_id,
        &body.capability,
        body.from,
        body.to,
    ) {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

async fn end_degradation(
    State(store): State<AppState>,
    Json(body): Json<EndClosureQuery>,
) -> impl IntoResponse {
    match store.end_degradation(&body.version, &body.event_id, body.at) {
        Ok(ended_at) => Json(EndClosureResponse { ended_at }).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, e).into_response(),
    }
}

#[derive(Serialize)]
struct SnapshotResponse {
    snapshot_id: String,
    catalog_version: String,
    created_at: DateTime<Utc>,
    result: RouteResult,
}

async fn get_snapshot(
    State(store): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match store.get_snapshot(&id) {
        Ok(Some(rec)) => Json(SnapshotResponse {
            snapshot_id: rec.snapshot_id,
            catalog_version: rec.catalog_version,
            created_at: rec.created_at,
            result: rec.result,
        })
        .into_response(),
        Ok(None) => err(StatusCode::NOT_FOUND, "snapshot not found").into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
