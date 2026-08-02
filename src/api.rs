//! Axum HTTP API. Every route request reads exactly one catalog version
//! inside a single read transaction, so a request never observes a mixture
//! of two versions during a hot reload. Snapshots are persisted verbatim.

use crate::db::{self, DbError, Pool};
use crate::model::*;
use crate::routing;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub pool: Arc<Pool>,
}

pub fn build_app(pool: Pool) -> Router {
    let state = AppState {
        pool: Arc::new(pool),
    };
    Router::new()
        .route("/health", get(health))
        .route("/catalog/import", post(import_catalog))
        .route("/catalog/active", get(active_catalog))
        .route("/catalog/versions", get(list_versions))
        .route("/route", post(route_single))
        .route("/route/batch", post(route_batch))
        .route("/snapshots/{id}", get(get_snapshot))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

enum ApiError {
    BadRequest(String),
    Conflict(String),
    NotFound(String),
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match self {
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m),
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, Json(ErrorBody { error: msg })).into_response()
    }
}

impl From<DbError> for ApiError {
    fn from(e: DbError) -> Self {
        match e {
            DbError::DuplicateVersion(v) => {
                ApiError::Conflict(format!("catalog version already exists: {v}"))
            }
            DbError::NoActiveCatalog => {
                ApiError::Conflict("no active catalog version; import one first".into())
            }
            DbError::BadTime(m) => ApiError::BadRequest(format!("invalid timestamp: {m}")),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl From<r2d2_sqlite::rusqlite::Error> for ApiError {
    fn from(e: r2d2_sqlite::rusqlite::Error) -> Self {
        ApiError::Internal(e.to_string())
    }
}

impl From<r2d2::Error> for ApiError {
    fn from(e: r2d2::Error) -> Self {
        ApiError::Internal(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Catalog administration
// ---------------------------------------------------------------------------

async fn import_catalog(
    State(st): State<AppState>,
    Json(cat): Json<CatalogImport>,
) -> Result<impl IntoResponse, ApiError> {
    if cat.catalog_version.trim().is_empty() {
        return Err(ApiError::BadRequest("catalogVersion must not be empty".into()));
    }
    if cat.points.is_empty() {
        return Err(ApiError::BadRequest("catalog must contain at least one point".into()));
    }
    let pool = st.pool.clone();
    let resp = tokio::task::spawn_blocking(move || db::import_catalog(&pool, &cat))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))??;
    Ok((StatusCode::CREATED, Json(resp)))
}

async fn active_catalog(State(st): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let pool = st.pool.clone();
    let info = tokio::task::spawn_blocking(move || -> Result<_, DbError> {
        let conn = pool.get()?;
        db::active_catalog_info(&conn)
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))??;
    match info {
        Some(i) => Ok(Json(i).into_response()),
        None => Err(ApiError::from(DbError::NoActiveCatalog)),
    }
}

#[derive(Serialize)]
struct VersionEntry {
    version: String,
    active: bool,
    #[serde(rename = "importedAt")]
    imported_at: String,
}

async fn list_versions(State(st): State<AppState>) -> Result<impl IntoResponse, ApiError> {
    let pool = st.pool.clone();
    let versions = tokio::task::spawn_blocking(move || -> Result<Vec<VersionEntry>, DbError> {
        let conn = pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT version, is_active, imported_at FROM catalog_versions ORDER BY imported_at",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(VersionEntry {
                    version: r.get(0)?,
                    active: r.get::<_, i64>(1)? == 1,
                    imported_at: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))??;
    Ok(Json(versions))
}

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// Validate and normalize a route request against the catalog. Normalization
/// sorts/dedups communication needs so permuted inputs yield identical
/// snapshots.
fn normalize_request(cat: &Catalog, req: &RouteRequest) -> Result<NormalizedRequest, ApiError> {
    if req.service_need.trim().is_empty() {
        return Err(ApiError::BadRequest("serviceNeed must not be empty".into()));
    }
    let known_need = |n: &str| {
        n == STANDARD_MOBILITY
            || cat.hard_requirements.contains_key(n)
            || cat
                .home_service
                .as_ref()
                .map(|h| h.allowed_mobility.iter().any(|m| m == n))
                .unwrap_or(false)
    };
    if !known_need(&req.mobility) {
        return Err(ApiError::BadRequest(format!(
            "unknown mobility need: {}",
            req.mobility
        )));
    }
    for c in &req.communication {
        if !known_need(c) {
            return Err(ApiError::BadRequest(format!(
                "unknown communication need: {c}"
            )));
        }
    }
    let at_epoch = match &req.at {
        Some(s) => db::parse_rfc3339(s)?,
        None => chrono::Utc::now().timestamp(),
    };
    let mut communication = req.communication.clone();
    communication.sort();
    communication.dedup();
    Ok(NormalizedRequest {
        service_need: req.service_need.clone(),
        mobility: req.mobility.clone(),
        communication,
        origin: req.origin,
        at: db::format_rfc3339(at_epoch),
        at_epoch,
    })
}

/// Core single-route pipeline shared by /route and /route/batch. Loads the
/// active catalog inside one read transaction, computes the outcome, then
/// persists the snapshot in a short write transaction.
fn route_one(pool: &Pool, req: &RouteRequest) -> Result<RouteOutcome, ApiError> {
    let conn = pool.get()?;
    // Read transaction pins the catalog view for this whole request.
    let tx = conn.unchecked_transaction()?;
    let version = db::active_version(&tx)?.ok_or(DbError::NoActiveCatalog)?;
    let cat = db::load_catalog(&tx, &version)?;
    let normalized = normalize_request(&cat, req)?;
    let snapshot_id = uuid::Uuid::new_v4().to_string();
    let outcome = routing::route(&cat, &normalized, snapshot_id.clone());
    let response_json = serde_json::to_string(&outcome)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    let request_json = serde_json::to_string(&normalized)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    tx.commit()?;
    db::save_snapshot(
        &conn,
        &snapshot_id,
        &cat.version,
        "single",
        &request_json,
        &response_json,
    )?;
    Ok(outcome)
}

async fn route_single(
    State(st): State<AppState>,
    Json(req): Json<RouteRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let pool = st.pool.clone();
    let outcome = tokio::task::spawn_blocking(move || route_one(&pool, &req))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))??;
    Ok(Json(outcome))
}

async fn route_batch(
    State(st): State<AppState>,
    Json(batch): Json<BatchRouteRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if batch.requests.is_empty() {
        return Err(ApiError::BadRequest("batch must contain at least one request".into()));
    }
    let pool = st.pool.clone();
    let resp = tokio::task::spawn_blocking(move || -> Result<BatchRouteResponse, ApiError> {
        let conn = pool.get()?;
        // The whole batch observes one catalog version in one read txn.
        let tx = conn.unchecked_transaction()?;
        let version = db::active_version(&tx)?.ok_or(DbError::NoActiveCatalog)?;
        let cat = db::load_catalog(&tx, &version)?;

        let mut normalized = Vec::with_capacity(batch.requests.len());
        for r in &batch.requests {
            normalized.push(normalize_request(&cat, r)?);
        }
        let batch_id = uuid::Uuid::new_v4().to_string();
        let mut snapshots = Vec::with_capacity(normalized.len());
        for n in normalized {
            let snapshot_id = uuid::Uuid::new_v4().to_string();
            snapshots.push(routing::route(&cat, &n, snapshot_id));
        }
        tx.commit()?;

        for s in &snapshots {
            let response_json = serde_json::to_string(s)
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            let request_json = serde_json::to_string(&s.request)
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            db::save_snapshot(
                &conn,
                &s.snapshot_id,
                &cat.version,
                "batch",
                &request_json,
                &response_json,
            )?;
        }
        Ok(BatchRouteResponse {
            batch_id,
            catalog_version: cat.version.clone(),
            snapshots,
        })
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))??;
    Ok(Json(resp))
}

// ---------------------------------------------------------------------------
// Snapshot replay: returns the stored response verbatim, never recomputed.
// ---------------------------------------------------------------------------

async fn get_snapshot(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let pool = st.pool.clone();
    let lookup = id.clone();
    let body = tokio::task::spawn_blocking(move || -> Result<_, DbError> {
        let conn = pool.get()?;
        db::load_snapshot(&conn, &lookup)
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))??;
    match body {
        Some(json) => Ok((
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response()),
        None => Err(ApiError::NotFound(format!("snapshot not found: {id}"))),
    }
}
