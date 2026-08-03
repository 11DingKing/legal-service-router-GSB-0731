use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::catalog::{parse_catalog, Catalog};
use crate::db::{
    get_active_version, get_tie_break, import_catalog, load_catalog, load_snapshot, save_snapshot,
    DbPool,
};
use crate::error::{AppError, AppResult};
use crate::routing::{
    compute, resolve_query_time, validate_request, RouteRequest, RouteResponse, RouteResult,
};

#[derive(Clone)]
pub struct AppState {
    pub pool: Arc<DbPool>,
    catalog_cache: Arc<RwLock<HashMap<String, Arc<Catalog>>>>,
}

fn load_catalog_cached(state: &AppState, version: &str) -> AppResult<Arc<Catalog>> {
    if let Some(cat) = state
        .catalog_cache
        .read()
        .expect("catalog cache lock poisoned")
        .get(version)
    {
        return Ok(Arc::clone(cat));
    }

    let cat = Arc::new(load_catalog(&state.pool, version)?);
    let mut cache = state
        .catalog_cache
        .write()
        .expect("catalog cache lock poisoned");
    cache.insert(version.to_string(), Arc::clone(&cat));
    Ok(cat)
}

pub fn build_router(pool: DbPool) -> Router {
    let state = AppState {
        pool: Arc::new(pool),
        catalog_cache: Arc::new(RwLock::new(HashMap::new())),
    };

    Router::new()
        .route("/health", get(health))
        .route("/admin/import", post(import_catalog_handler))
        .route("/admin/active", get(active_version_handler))
        .route("/route", post(route_handler))
        .route("/route/batch", post(route_batch_handler))
        .route("/snapshots/:id", get(get_snapshot_handler))
        .with_state(state)
}

async fn health() -> StatusCode {
    StatusCode::OK
}

async fn import_catalog_handler(
    State(state): State<AppState>,
    body: String,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let catalog = parse_catalog(&body)?;
    let point_count = catalog.points.len();
    let version = catalog.catalog_version.clone();

    let pool = Arc::clone(&state.pool);
    let version_for_check = version.clone();
    let already_exists = {
        let pool = Arc::clone(&pool);
        tokio::task::spawn_blocking(move || {
            crate::db::version_exists(&pool, &version_for_check)
        })
        .await
        .map_err(|e| AppError::Internal(e.to_string()))??
    };

    if already_exists {
        return Err(AppError::Conflict(format!(
            "catalog version '{version}' already exists"
        )));
    }

    let state_clone = state.clone();
    let version_for_import = version.clone();
    let cat = catalog.clone();
    tokio::task::spawn_blocking(move || -> AppResult<()> {
        import_catalog(&state_clone.pool, &cat)?;
        let loaded = load_catalog(&state_clone.pool, &version_for_import)?;
        state_clone
            .catalog_cache
            .write()
            .expect("catalog cache lock poisoned")
            .insert(version_for_import, Arc::new(loaded));
        Ok(())
    })
    .await
    .map_err(|e| AppError::Internal(e.to_string()))??;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "catalogVersion": version,
            "imported": true,
            "pointCount": point_count,
            "active": true,
        })),
    ))
}

async fn active_version_handler(
    State(state): State<AppState>,
) -> AppResult<Json<serde_json::Value>> {
    let pool = Arc::clone(&state.pool);
    let version = tokio::task::spawn_blocking(move || get_active_version(&pool))
        .await
        .map_err(|e| AppError::Internal(e.to_string()))??;

    Ok(Json(serde_json::json!({
        "catalogVersion": version,
    })))
}

fn build_response(
    snapshot_id: String,
    catalog_version: String,
    req: &RouteRequest,
    query_time: &chrono::DateTime<chrono::Utc>,
    result: RouteResult,
) -> RouteResponse {
    RouteResponse {
        snapshot_id,
        catalog_version,
        query_time: query_time.to_rfc3339(),
        origin_grid: req.origin_grid,
        service: req.service.clone(),
        home_service: result.home_service,
        candidates: result.candidates,
        excluded: result.excluded,
        tie_break: result.tie_break,
    }
}

async fn route_handler(
    State(state): State<AppState>,
    Json(req): Json<RouteRequest>,
) -> AppResult<Json<RouteResponse>> {
    validate_request(&req)?;
    let query_time = resolve_query_time(&req)?;

    let state_clone = state.clone();
    let req_clone = req.clone();
    let resp = tokio::task::spawn_blocking(move || -> AppResult<RouteResponse> {
        let version = get_active_version(&state_clone.pool)?
            .ok_or_else(|| AppError::BadRequest("no active catalog imported".to_string()))?;
        let catalog = load_catalog_cached(&state_clone, &version)?;
        let mut result = compute(&catalog, &req_clone, query_time);
        let snapshot_id =
            save_snapshot(&state_clone.pool, &req_clone, &version, &query_time, &mut result)?;
        Ok(build_response(
            snapshot_id,
            version,
            &req_clone,
            &query_time,
            result,
        ))
    })
    .await
    .map_err(|e| AppError::Internal(e.to_string()))??;

    Ok(Json(resp))
}

#[derive(Debug, Deserialize)]
struct BatchRequest {
    requests: Vec<RouteRequest>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BatchResponse {
    catalog_version: String,
    results: Vec<BatchItem>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BatchItem {
    snapshot_id: String,
    response: RouteResponse,
}

async fn route_batch_handler(
    State(state): State<AppState>,
    Json(batch): Json<BatchRequest>,
) -> AppResult<Json<BatchResponse>> {
    if batch.requests.is_empty() {
        return Err(AppError::BadRequest(
            "batch must contain at least one request".to_string(),
        ));
    }
    for r in &batch.requests {
        validate_request(r)?;
    }

    let state_clone = state.clone();
    let requests = batch.requests;
    let batch_resp = tokio::task::spawn_blocking(move || -> AppResult<BatchResponse> {
        let version = get_active_version(&state_clone.pool)?
            .ok_or_else(|| AppError::BadRequest("no active catalog imported".to_string()))?;
        let catalog = load_catalog_cached(&state_clone, &version)?;

        let mut results = Vec::with_capacity(requests.len());
        for req in &requests {
            let query_time = resolve_query_time(req)?;
            let mut result = compute(&catalog, req, query_time);
            let snapshot_id =
                save_snapshot(&state_clone.pool, req, &version, &query_time, &mut result)?;
            let resp = build_response(
                snapshot_id.clone(),
                version.clone(),
                req,
                &query_time,
                result,
            );
            results.push(BatchItem {
                snapshot_id,
                response: resp,
            });
        }

        Ok(BatchResponse {
            catalog_version: version,
            results,
        })
    })
    .await
    .map_err(|e| AppError::Internal(e.to_string()))??;

    Ok(Json(batch_resp))
}

async fn get_snapshot_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<Json<RouteResponse>> {
    let pool = Arc::clone(&state.pool);
    let resp = tokio::task::spawn_blocking(move || -> AppResult<RouteResponse> {
        let (req, version, query_time, mut result) = load_snapshot(&pool, &id)?;
        result.tie_break = get_tie_break(&pool, &version)?;
        Ok(build_response(
            id,
            version,
            &req,
            &query_time,
            result,
        ))
    })
    .await
    .map_err(|e| AppError::Internal(e.to_string()))??;

    Ok(Json(resp))
}
