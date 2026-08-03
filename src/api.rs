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
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Db>,
}

pub fn app(db: Arc<Db>) -> Router {
    Router::new()
        .route("/catalog", post(import_catalog).get(get_catalog))
        .route("/route", post(route_single))
        .route("/route/batch", post(route_batch))
        .route("/snapshots/:id", get(get_snapshot))
        .with_state(AppState { db })
}

async fn import_catalog(
    State(state): State<AppState>,
    Json(catalog): Json<Catalog>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    state
        .db
        .import_catalog(&catalog)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
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

fn route_using_current(state: &AppState, request: &RouteRequest) -> Result<RouteResponse, (StatusCode, String)> {
    let catalog = state
        .db
        .load_current_catalog()
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::BAD_REQUEST, "no catalog imported".to_string()))?;
    let snapshot_id = Uuid::new_v4().to_string();
    let response = router::route(&catalog, request, snapshot_id.clone());
    let request_payload = serde_json::to_string(request)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let result_payload = serde_json::to_string(&response)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    state
        .db
        .save_snapshot(
            &snapshot_id,
            &catalog.catalog_version,
            &request_payload,
            &result_payload,
        )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(response)
}

async fn route_single(
    State(state): State<AppState>,
    Json(request): Json<RouteRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let response = route_using_current(&state, &request)?;
    Ok(Json(response))
}

async fn route_batch(
    State(state): State<AppState>,
    Json(batch): Json<BatchRouteRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let mut results = Vec::with_capacity(batch.requests.len());
    for request in &batch.requests {
        results.push(route_using_current(&state, request)?);
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
