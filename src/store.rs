//! Persistence and the versioned in-memory catalog cache.
//!
//! # Schema
//!
//! SQLite is the source of truth. A catalog version is imported once into
//! `catalog_versions` (raw JSON kept for auditability) and expanded into
//! relational tables: `points`, `point_services`, `point_access`,
//! `hard_requirements`, `home_service`, `home_mobility`, and `closures`.
//! Routing snapshots are stored verbatim in `snapshots`.
//!
//! Closures are their own table (not baked into the raw blob) so the
//! start/end endpoints can toggle a temporary closure without rewriting the
//! catalog. Every closure mutation rebuilds that version's normalized
//! [`Catalog`] and atomically swaps a fresh `Arc<Catalog>` into the cache.
//!
//! # Consistency model
//!
//! The cache holds `Arc<Catalog>` per version plus an `active` pointer, guarded
//! by a `RwLock`. A read (routing) clones the active `Arc` under a short read
//! lock and then releases it. Because a `Catalog` is immutable, the request
//! keeps a complete, self-consistent version for its whole lifetime even if a
//! concurrent import or closure toggle swaps in a newer `Arc`. Old snapshots
//! are replayed from stored JSON and therefore never mix in new data.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use crate::model::{Catalog, Closure, RawCatalog};
use crate::routing::{RouteRequest, RouteResult};

/// Snapshot rows returned on replay.
#[derive(Debug, Clone)]
pub struct SnapshotRecord {
    pub snapshot_id: String,
    pub catalog_version: String,
    pub created_at: DateTime<Utc>,
    pub result: RouteResult,
}

struct Cache {
    versions: BTreeMap<String, Arc<Catalog>>,
    active: Option<String>,
}

/// The application store: a serialized SQLite connection plus the versioned
/// catalog cache.
pub struct Store {
    conn: std::sync::Mutex<Connection>,
    cache: RwLock<Cache>,
}

impl Store {
    /// Open (or create) a store at `path`. Use `":memory:"` for tests.
    pub fn open(path: &str) -> Result<Store, String> {
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        Store::init(conn)
    }

    fn init(conn: Connection) -> Result<Store, String> {
        conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
        let store = Store {
            conn: std::sync::Mutex::new(conn),
            cache: RwLock::new(Cache { versions: BTreeMap::new(), active: None }),
        };
        store.reload_all_from_db()?;
        Ok(store)
    }

    /// Rebuild the entire cache from the database (used on startup).
    fn reload_all_from_db(&self) -> Result<(), String> {
        let conn = self.conn.lock().unwrap();
        let mut versions: Vec<String> = Vec::new();
        {
            let mut stmt = conn
                .prepare("SELECT version FROM catalog_versions ORDER BY version")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|e| e.to_string())?;
            for v in rows {
                versions.push(v.map_err(|e| e.to_string())?);
            }
        }
        let active: Option<String> = conn
            .query_row(
                "SELECT version FROM active_version WHERE id = 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok();

        let mut map = BTreeMap::new();
        for v in &versions {
            let cat = Self::build_catalog(&conn, v)?;
            map.insert(v.clone(), Arc::new(cat));
        }
        drop(conn);

        let mut cache = self.cache.write().unwrap();
        cache.versions = map;
        cache.active = active;
        Ok(())
    }

    /// Import a catalog version. This is the hot-reload primitive: it persists a
    /// new immutable version and swaps its `Arc` into the cache. Re-importing an
    /// existing version is rejected so snapshots stay reproducible.
    pub fn import_catalog(&self, raw_json: &[u8], activate: bool) -> Result<String, String> {
        let catalog = Catalog::from_json(raw_json)?;
        let version = catalog.version.clone();

        {
            let mut conn = self.conn.lock().unwrap();
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM catalog_versions WHERE version = ?1",
                    params![version],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            if exists {
                return Err(format!("catalog version already imported: {version}"));
            }

            let raw: RawCatalog =
                serde_json::from_slice(raw_json).map_err(|e| e.to_string())?;
            let tx = conn.transaction().map_err(|e| e.to_string())?;
            let now = Utc::now().to_rfc3339();
            tx.execute(
                "INSERT INTO catalog_versions(version, raw_json, cost_formula, tie_break, imported_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    version,
                    String::from_utf8_lossy(raw_json).to_string(),
                    raw.cost_formula,
                    serde_json::to_string(&raw.tie_break).unwrap(),
                    now
                ],
            )
            .map_err(|e| e.to_string())?;

            for p in &raw.points {
                tx.execute(
                    "INSERT INTO points(version, id, grid_x, grid_y, barrier_penalty) VALUES (?1,?2,?3,?4,?5)",
                    params![version, p.id, p.grid[0], p.grid[1], p.barrier_penalty],
                )
                .map_err(|e| e.to_string())?;
                for s in &p.services {
                    tx.execute(
                        "INSERT INTO point_services(version, point_id, service) VALUES (?1,?2,?3)",
                        params![version, p.id, s],
                    )
                    .map_err(|e| e.to_string())?;
                }
                for a in &p.access {
                    tx.execute(
                        "INSERT INTO point_access(version, point_id, capability) VALUES (?1,?2,?3)",
                        params![version, p.id, a],
                    )
                    .map_err(|e| e.to_string())?;
                }
            }

            for (need, cap) in &raw.hard_requirements {
                tx.execute(
                    "INSERT INTO hard_requirements(version, need_key, capability) VALUES (?1,?2,?3)",
                    params![version, need, cap],
                )
                .map_err(|e| e.to_string())?;
            }

            for c in &raw.closures {
                tx.execute(
                    "INSERT INTO closures(version, event_id, point_id, from_ts, to_ts) VALUES (?1,?2,?3,?4,?5)",
                    params![version, c.event_id, c.point_id, c.from.to_rfc3339(), c.to.to_rfc3339()],
                )
                .map_err(|e| e.to_string())?;
            }

            if let Some(h) = &raw.home_service {
                tx.execute(
                    "INSERT INTO home_service(version, allowed_service, reason) VALUES (?1,?2,?3)",
                    params![version, h.allowed_service, h.reason],
                )
                .map_err(|e| e.to_string())?;
                for m in &h.allowed_mobility {
                    tx.execute(
                        "INSERT INTO home_mobility(version, mobility) VALUES (?1,?2)",
                        params![version, m],
                    )
                    .map_err(|e| e.to_string())?;
                }
            }

            if activate {
                tx.execute(
                    "INSERT INTO active_version(id, version) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET version = ?1",
                    params![version],
                )
                .map_err(|e| e.to_string())?;
            }

            tx.commit().map_err(|e| e.to_string())?;
        }

        // Rebuild just this version and swap into cache atomically.
        let cat = {
            let conn = self.conn.lock().unwrap();
            Self::build_catalog(&conn, &version)?
        };
        let mut cache = self.cache.write().unwrap();
        cache.versions.insert(version.clone(), Arc::new(cat));
        if activate {
            cache.active = Some(version.clone());
        }
        Ok(version)
    }

    /// Set the active catalog version (a form of hot reload / rollback).
    pub fn activate(&self, version: &str) -> Result<(), String> {
        {
            let cache = self.cache.read().unwrap();
            if !cache.versions.contains_key(version) {
                return Err(format!("unknown catalog version: {version}"));
            }
        }
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO active_version(id, version) VALUES (1, ?1) ON CONFLICT(id) DO UPDATE SET version = ?1",
                params![version],
            )
            .map_err(|e| e.to_string())?;
        }
        self.cache.write().unwrap().active = Some(version.to_string());
        Ok(())
    }

    /// Start a temporary closure on the active version's point. Rebuilds and
    /// swaps the version's `Arc` atomically. `event_id` must be unique.
    pub fn start_closure(
        &self,
        version: &str,
        event_id: &str,
        point_id: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<(), String> {
        if to < from {
            return Err("closure `to` precedes `from`".to_string());
        }
        {
            let conn = self.conn.lock().unwrap();
            let point_exists: bool = conn
                .query_row(
                    "SELECT 1 FROM points WHERE version = ?1 AND id = ?2",
                    params![version, point_id],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            if !point_exists {
                return Err(format!("unknown point {point_id} in version {version}"));
            }
            conn.execute(
                "INSERT INTO closures(version, event_id, point_id, from_ts, to_ts) VALUES (?1,?2,?3,?4,?5)",
                params![version, event_id, point_id, from.to_rfc3339(), to.to_rfc3339()],
            )
            .map_err(|e| {
                if e.to_string().contains("UNIQUE") {
                    format!("closure event already exists: {event_id}")
                } else {
                    e.to_string()
                }
            })?;
        }
        self.rebuild_version(version)
    }

    /// End a temporary closure at instant `at` (defaults to now) by clamping its
    /// `to` bound. With half-open `[from, to)` semantics the point re-opens
    /// exactly at `at`. Returns the effective end instant.
    pub fn end_closure(
        &self,
        version: &str,
        event_id: &str,
        at: Option<DateTime<Utc>>,
    ) -> Result<DateTime<Utc>, String> {
        let at = at.unwrap_or_else(Utc::now);
        {
            let conn = self.conn.lock().unwrap();
            let from_ts: Option<String> = conn
                .query_row(
                    "SELECT from_ts FROM closures WHERE version = ?1 AND event_id = ?2",
                    params![version, event_id],
                    |r| r.get(0),
                )
                .ok();
            let from_ts = from_ts
                .ok_or_else(|| format!("unknown closure event: {event_id}"))?;
            let from = DateTime::parse_from_rfc3339(&from_ts)
                .map_err(|e| e.to_string())?
                .with_timezone(&Utc);
            // Clamp so the interval never inverts: ending before it starts
            // yields an empty, always-open interval.
            let effective = if at < from { from } else { at };
            conn.execute(
                "UPDATE closures SET to_ts = ?3 WHERE version = ?1 AND event_id = ?2",
                params![version, event_id, effective.to_rfc3339()],
            )
            .map_err(|e| e.to_string())?;
        }
        self.rebuild_version(version)?;
        Ok(at)
    }

    fn rebuild_version(&self, version: &str) -> Result<(), String> {
        let cat = {
            let conn = self.conn.lock().unwrap();
            Self::build_catalog(&conn, version)?
        };
        self.cache
            .write()
            .unwrap()
            .versions
            .insert(version.to_string(), Arc::new(cat));
        Ok(())
    }

    /// Capture the active version's immutable catalog. The `Arc` is cloned under
    /// a brief read lock and returned, giving the caller a stable, complete
    /// version for the rest of the request.
    pub fn active_catalog(&self) -> Option<Arc<Catalog>> {
        let cache = self.cache.read().unwrap();
        let v = cache.active.as_ref()?;
        cache.versions.get(v).cloned()
    }

    /// Capture a specific catalog version's immutable `Arc`.
    pub fn catalog(&self, version: &str) -> Option<Arc<Catalog>> {
        self.cache.read().unwrap().versions.get(version).cloned()
    }

    /// List known versions and which one is active.
    pub fn list_versions(&self) -> (Vec<String>, Option<String>) {
        let cache = self.cache.read().unwrap();
        (cache.versions.keys().cloned().collect(), cache.active.clone())
    }

    /// Persist a routing result as an immutable snapshot; returns its id.
    pub fn save_snapshot(&self, req: &RouteRequest, result: &RouteResult) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO snapshots(snapshot_id, catalog_version, request_json, result_json, created_at) VALUES (?1,?2,?3,?4,?5)",
            params![
                id,
                result.catalog_version,
                serde_json::to_string(&SavedRequest::from(req)).unwrap(),
                serde_json::to_string(result).unwrap(),
                Utc::now().to_rfc3339()
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(id)
    }

    /// Replay a stored snapshot verbatim. Never re-runs routing, so it cannot
    /// pick up catalog changes made after the snapshot was taken.
    pub fn get_snapshot(&self, snapshot_id: &str) -> Result<Option<SnapshotRecord>, String> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT snapshot_id, catalog_version, result_json, created_at FROM snapshots WHERE snapshot_id = ?1",
            params![snapshot_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        );
        match row {
            Ok((sid, version, result_json, created_at)) => {
                let result: RouteResult =
                    serde_json::from_str(&result_json).map_err(|e| e.to_string())?;
                let created_at = DateTime::parse_from_rfc3339(&created_at)
                    .map_err(|e| e.to_string())?
                    .with_timezone(&Utc);
                Ok(Some(SnapshotRecord {
                    snapshot_id: sid,
                    catalog_version: version,
                    created_at,
                    result,
                }))
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Build a normalized [`Catalog`] for `version` from the relational tables.
    fn build_catalog(conn: &Connection, version: &str) -> Result<Catalog, String> {
        use crate::model::{Grid, HomeService, Point};
        use std::collections::{BTreeMap, BTreeSet};

        let (cost_formula, tie_break_json): (String, String) = conn
            .query_row(
                "SELECT cost_formula, tie_break FROM catalog_versions WHERE version = ?1",
                params![version],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| e.to_string())?;
        let tie_break: Vec<String> = serde_json::from_str(&tie_break_json).unwrap_or_default();

        // Points.
        let mut points: BTreeMap<String, Point> = BTreeMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT id, grid_x, grid_y, barrier_penalty FROM points WHERE version = ?1")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![version], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                let (id, gx, gy, bp) = row.map_err(|e| e.to_string())?;
                points.insert(
                    id.clone(),
                    Point {
                        id,
                        grid: Grid { x: gx, y: gy },
                        services: BTreeSet::new(),
                        access: BTreeSet::new(),
                        barrier_penalty: bp,
                    },
                );
            }
        }
        {
            let mut stmt = conn
                .prepare("SELECT point_id, service FROM point_services WHERE version = ?1")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![version], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                let (pid, s) = row.map_err(|e| e.to_string())?;
                if let Some(p) = points.get_mut(&pid) {
                    p.services.insert(s);
                }
            }
        }
        {
            let mut stmt = conn
                .prepare("SELECT point_id, capability FROM point_access WHERE version = ?1")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![version], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                let (pid, a) = row.map_err(|e| e.to_string())?;
                if let Some(p) = points.get_mut(&pid) {
                    p.access.insert(a);
                }
            }
        }

        // Hard requirements.
        let mut hard_requirements: BTreeMap<String, String> = BTreeMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT need_key, capability FROM hard_requirements WHERE version = ?1")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![version], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                let (k, v) = row.map_err(|e| e.to_string())?;
                hard_requirements.insert(k, v);
            }
        }

        // Closures.
        let mut closures_by_point: BTreeMap<String, Vec<Closure>> = BTreeMap::new();
        {
            let mut stmt = conn
                .prepare("SELECT event_id, point_id, from_ts, to_ts FROM closures WHERE version = ?1 ORDER BY event_id")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![version], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                let (event_id, point_id, from_ts, to_ts) = row.map_err(|e| e.to_string())?;
                let from = DateTime::parse_from_rfc3339(&from_ts)
                    .map_err(|e| e.to_string())?
                    .with_timezone(&Utc);
                let to = DateTime::parse_from_rfc3339(&to_ts)
                    .map_err(|e| e.to_string())?
                    .with_timezone(&Utc);
                closures_by_point
                    .entry(point_id.clone())
                    .or_default()
                    .push(Closure { event_id, point_id, from, to });
            }
        }

        // Home service.
        let home_service = {
            let hs: Option<(String, String)> = conn
                .query_row(
                    "SELECT allowed_service, reason FROM home_service WHERE version = ?1",
                    params![version],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            match hs {
                Some((allowed_service, reason)) => {
                    let mut allowed_mobility = BTreeSet::new();
                    let mut stmt = conn
                        .prepare("SELECT mobility FROM home_mobility WHERE version = ?1")
                        .map_err(|e| e.to_string())?;
                    let rows = stmt
                        .query_map(params![version], |r| r.get::<_, String>(0))
                        .map_err(|e| e.to_string())?;
                    for m in rows {
                        allowed_mobility.insert(m.map_err(|e| e.to_string())?);
                    }
                    Some(HomeService { allowed_service, allowed_mobility, reason })
                }
                None => None,
            }
        };

        Ok(Catalog {
            version: version.to_string(),
            cost_formula,
            points,
            hard_requirements,
            tie_break,
            closures_by_point,
            home_service,
        })
    }
}

/// Minimal request echo stored alongside a snapshot for auditing.
#[derive(serde::Serialize)]
struct SavedRequest {
    origin: [i64; 2],
    service: String,
    mobility: Option<String>,
    communication: Vec<String>,
    at: Option<DateTime<Utc>>,
}

impl From<&RouteRequest> for SavedRequest {
    fn from(r: &RouteRequest) -> Self {
        SavedRequest {
            origin: r.origin,
            service: r.service.clone(),
            mobility: r.mobility.clone(),
            communication: r.communication.clone(),
            at: r.at,
        }
    }
}

const SCHEMA: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS catalog_versions (
    version      TEXT PRIMARY KEY,
    raw_json     TEXT NOT NULL,
    cost_formula TEXT NOT NULL DEFAULT '',
    tie_break    TEXT NOT NULL DEFAULT '[]',
    imported_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS points (
    version         TEXT NOT NULL,
    id              TEXT NOT NULL,
    grid_x          INTEGER NOT NULL,
    grid_y          INTEGER NOT NULL,
    barrier_penalty INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (version, id)
);

CREATE TABLE IF NOT EXISTS point_services (
    version   TEXT NOT NULL,
    point_id  TEXT NOT NULL,
    service   TEXT NOT NULL,
    PRIMARY KEY (version, point_id, service)
);

CREATE TABLE IF NOT EXISTS point_access (
    version    TEXT NOT NULL,
    point_id   TEXT NOT NULL,
    capability TEXT NOT NULL,
    PRIMARY KEY (version, point_id, capability)
);

CREATE TABLE IF NOT EXISTS hard_requirements (
    version    TEXT NOT NULL,
    need_key   TEXT NOT NULL,
    capability TEXT NOT NULL,
    PRIMARY KEY (version, need_key)
);

CREATE TABLE IF NOT EXISTS closures (
    version   TEXT NOT NULL,
    event_id  TEXT NOT NULL,
    point_id  TEXT NOT NULL,
    from_ts   TEXT NOT NULL,
    to_ts     TEXT NOT NULL,
    PRIMARY KEY (version, event_id)
);

CREATE TABLE IF NOT EXISTS home_service (
    version         TEXT PRIMARY KEY,
    allowed_service TEXT NOT NULL,
    reason          TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS home_mobility (
    version  TEXT NOT NULL,
    mobility TEXT NOT NULL,
    PRIMARY KEY (version, mobility)
);

CREATE TABLE IF NOT EXISTS active_version (
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    version TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS snapshots (
    snapshot_id     TEXT PRIMARY KEY,
    catalog_version TEXT NOT NULL,
    request_json    TEXT NOT NULL,
    result_json     TEXT NOT NULL,
    created_at      TEXT NOT NULL
);
"#;
