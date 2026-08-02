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
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Route cache key: (catalog version, normalized request JSON). Because the
/// version is part of the key and catalog versions are immutable, a hit can
/// never mix data from another version. Both caches are cleared on import.
type CatalogCache = Arc<Mutex<HashMap<String, Arc<Catalog>>>>;
type RouteCache = Arc<Mutex<HashMap<(String, String), RouteOutcome>>>;

#[derive(Clone)]
pub struct AppState {
    pub pool: Arc<Pool>,
    catalog_cache: CatalogCache,
    route_cache: RouteCache,
}

pub fn build_app(pool: Pool) -> Router {
    let state = AppState {
        pool: Arc::new(pool),
        catalog_cache: Arc::new(Mutex::new(HashMap::new())),
        route_cache: Arc::new(Mutex::new(HashMap::new())),
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
    // Hot reload: invalidate every cached catalog and route result so no
    // query can observe stale pre-import data.
    st.catalog_cache.lock().unwrap().clear();
    st.route_cache.lock().unwrap().clear();
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

/// Read the currently active catalog version. This single read is the
/// pinning point: everything downstream is keyed by this immutable version.
fn active_version_now(pool: &Pool) -> Result<String, ApiError> {
    let conn = pool.get()?;
    Ok(db::active_version(&conn)?.ok_or(DbError::NoActiveCatalog)?)
}

/// Catalogs are immutable per version, so they can be cached by version.
fn cached_catalog(st: &AppState, version: &str) -> Result<Arc<Catalog>, ApiError> {
    if let Some(c) = st.catalog_cache.lock().unwrap().get(version) {
        return Ok(c.clone());
    }
    let conn = st.pool.get()?;
    let cat = Arc::new(db::load_catalog(&conn, version)?);
    st.catalog_cache
        .lock()
        .unwrap()
        .insert(version.to_string(), cat.clone());
    Ok(cat)
}

/// Compute (or reuse) the outcome for one normalized request pinned to one
/// catalog version, then persist it as a fresh immutable snapshot. Cache
/// hits still mint a new snapshotId and a new stored snapshot.
fn outcome_for(
    st: &AppState,
    cat: &Arc<Catalog>,
    normalized: &NormalizedRequest,
    kind: &str,
) -> Result<RouteOutcome, ApiError> {
    let key = (
        cat.version.clone(),
        serde_json::to_string(normalized).map_err(|e| ApiError::Internal(e.to_string()))?,
    );
    let cached = st.route_cache.lock().unwrap().get(&key).cloned();
    let outcome = match cached {
        Some(mut o) => {
            o.snapshot_id = uuid::Uuid::new_v4().to_string();
            o
        }
        None => {
            let o = routing::route(cat, normalized, uuid::Uuid::new_v4().to_string());
            st.route_cache.lock().unwrap().insert(key, o.clone());
            o
        }
    };
    let response_json =
        serde_json::to_string(&outcome).map_err(|e| ApiError::Internal(e.to_string()))?;
    let request_json =
        serde_json::to_string(normalized).map_err(|e| ApiError::Internal(e.to_string()))?;
    let conn = st.pool.get()?;
    db::save_snapshot(
        &conn,
        &outcome.snapshot_id,
        &cat.version,
        kind,
        &request_json,
        &response_json,
    )?;
    Ok(outcome)
}

fn route_one(st: &AppState, req: &RouteRequest) -> Result<RouteOutcome, ApiError> {
    let version = active_version_now(&st.pool)?;
    let cat = cached_catalog(st, &version)?;
    let normalized = normalize_request(&cat, req)?;
    outcome_for(st, &cat, &normalized, "single")
}

async fn route_single(
    State(st): State<AppState>,
    Json(req): Json<RouteRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let outcome = tokio::task::spawn_blocking(move || route_one(&st, &req))
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
    let resp = tokio::task::spawn_blocking(move || -> Result<BatchRouteResponse, ApiError> {
        // Pin the whole batch to the one version active at this instant.
        let version = active_version_now(&st.pool)?;
        let cat = cached_catalog(&st, &version)?;

        let mut snapshots = Vec::with_capacity(batch.requests.len());
        for r in &batch.requests {
            let normalized = normalize_request(&cat, r)?;
            snapshots.push(outcome_for(&st, &cat, &normalized, "batch")?);
        }
        Ok(BatchRouteResponse {
            batch_id: uuid::Uuid::new_v4().to_string(),
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
