use crate::db::Db;
use crate::models::{
    BatchRouteRequest, BatchRouteResponse, Catalog, RouteRequest, RouteResponse,
};
use crate::router;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
    pub query_cache: Arc<Mutex<HashMap<String, RouteResponse>>>,
}

pub fn app(db: Arc<Db>) -> Router {
    let state = AppState {
        db,
        query_cache: Arc::new(Mutex::new(HashMap::new())),
    };
    Router::new()
        .route("/catalog", post(import_catalog).get(get_catalog))
        .route("/route", post(route_single))
        .route("/route/batch", post(route_batch))
        .route("/snapshots/:id", get(get_snapshot))
        .with_state(state)
}

fn cache_key(catalog_version: &str, request: &RouteRequest) -> String {
    let canonical = serde_json::to_string(request).unwrap_or_default();
    format!("{}::{}", catalog_version, canonical)
}

#[doc(hidden)]
pub fn compute_and_persist(
    state: &AppState,
    catalog: &Catalog,
    request: &RouteRequest,
) -> Result<RouteResponse, (StatusCode, String)> {
    compute_and_persist_inner(state, catalog, request)
}

fn compute_and_persist_inner(
    state: &AppState,
    catalog: &Catalog,
    request: &RouteRequest,
) -> Result<RouteResponse, (StatusCode, String)> {
    let key = cache_key(&catalog.catalog_version, request);

    if let Some(cached) = state
        .query_cache
        .lock()
        .expect("cache lock poisoned")
        .get(&key)
        .cloned()
    {
        let snapshot_id = Uuid::new_v4().to_string();
        let mut response = cached;
        response.snapshot_id = snapshot_id.clone();
        persist_snapshot(state, catalog, request, &snapshot_id, &response)?;
        return Ok(response);
    }

    let snapshot_id = Uuid::new_v4().to_string();
    let response = router::route(catalog, request, snapshot_id.clone());
    persist_snapshot(state, catalog, request, &snapshot_id, &response)?;

    state
        .query_cache
        .lock()
        .expect("cache lock poisoned")
        .insert(key, response.clone());
    Ok(response)
}

fn persist_snapshot(
    state: &AppState,
    catalog: &Catalog,
    request: &RouteRequest,
    snapshot_id: &str,
    response: &RouteResponse,
) -> Result<(), (StatusCode, String)> {
    let request_payload = serde_json::to_string(request)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let result_payload = serde_json::to_string(response)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state
        .db
        .save_snapshot(
            snapshot_id,
            &catalog.catalog_version,
            &request_payload,
            &result_payload,
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(())
}

async fn import_catalog(
    State(state): State<AppState>,
    Json(catalog): Json<Catalog>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    state
        .db
        .import_catalog(&catalog)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state.query_cache.lock().expect("cache lock poisoned").clear();
    Ok(Json(serde_json::json!({
        "catalogVersion": catalog.catalog_version,
        "status": "imported"
    })))
}

async fn get_catalog(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    match state
        .db
        .catalog_summary()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        Some(summary) => Ok(Json(summary)),
        None => Err((StatusCode::NOT_FOUND, "no catalog imported".to_string())),
    }
}

async fn route_single(
    State(state): State<AppState>,
    Json(request): Json<RouteRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let catalog = state
        .db
        .load_current_catalog()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::BAD_REQUEST, "no catalog imported".to_string()))?;
    let response = compute_and_persist(&state, &catalog, &request)?;
    Ok(Json(response))
}

async fn route_batch(
    State(state): State<AppState>,
    Json(batch): Json<BatchRouteRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let catalog = state
        .db
        .load_current_catalog()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::BAD_REQUEST, "no catalog imported".to_string()))?;
    let pinned_version = catalog.catalog_version.clone();
    let mut results = Vec::with_capacity(batch.requests.len());
    for request in &batch.requests {
        let response = compute_and_persist(&state, &catalog, request)?;
        assert_eq!(response.catalog_version, pinned_version);
        results.push(response);
    }
    Ok(Json(BatchRouteResponse { results }))
}

async fn get_snapshot(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let snapshot = state
        .db
        .load_snapshot(&id)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "snapshot not found".to_string()))?;
    let response: RouteResponse = serde_json::from_str(&snapshot.payload)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(response))
}
