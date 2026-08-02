//! SQLite persistence: schema, versioned catalog import (atomic hot reload),
//! per-version catalog load, and immutable routing snapshots.

use crate::model::*;
use r2d2_sqlite::rusqlite::{self, params, Connection, OptionalExtension};
use std::collections::BTreeMap;
use thiserror::Error;

pub type Pool = r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("pool error: {0}")]
    Pool(#[from] r2d2::Error),
    #[error("catalog version already exists: {0}")]
    DuplicateVersion(String),
    #[error("invalid RFC3339 timestamp: {0}")]
    BadTime(String),
    #[error("no active catalog version")]
    NoActiveCatalog,
}

pub fn parse_rfc3339(s: &str) -> Result<i64, DbError> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp())
        .map_err(|_| DbError::BadTime(s.to_string()))
}

pub fn format_rfc3339(epoch: i64) -> String {
    chrono::DateTime::from_timestamp(epoch, 0)
        .expect("valid epoch")
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Build a connection pool. WAL mode lets readers run concurrently with the
/// single writer used for hot reloads.
pub fn open_pool(path: &str) -> Result<Pool, DbError> {
    let manager = r2d2_sqlite::SqliteConnectionManager::file(path)
        .with_init(|c| {
            c.pragma_update(None, "journal_mode", "WAL")?;
            c.pragma_update(None, "foreign_keys", "ON")?;
            c.pragma_update(None, "busy_timeout", 5000_i64)
        });
    let pool = r2d2::Pool::builder().max_size(8).build(manager)?;
    let conn = pool.get()?;
    init_schema(&conn)?;
    Ok(pool)
}

pub fn init_schema(conn: &Connection) -> Result<(), DbError> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS catalog_versions (
            version      TEXT PRIMARY KEY,
            cost_formula TEXT,
            imported_at  TEXT NOT NULL,
            is_active    INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS service_points (
            version         TEXT NOT NULL REFERENCES catalog_versions(version),
            point_id        TEXT NOT NULL,
            grid_x          INTEGER NOT NULL,
            grid_y          INTEGER NOT NULL,
            barrier_penalty INTEGER NOT NULL,
            PRIMARY KEY (version, point_id)
        );
        CREATE TABLE IF NOT EXISTS point_services (
            version  TEXT NOT NULL,
            point_id TEXT NOT NULL,
            service  TEXT NOT NULL,
            PRIMARY KEY (version, point_id, service)
        );
        CREATE TABLE IF NOT EXISTS point_access (
            version  TEXT NOT NULL,
            point_id TEXT NOT NULL,
            access   TEXT NOT NULL,
            PRIMARY KEY (version, point_id, access)
        );
        CREATE TABLE IF NOT EXISTS hard_requirements (
            version TEXT NOT NULL,
            need    TEXT NOT NULL,
            access  TEXT NOT NULL,
            PRIMARY KEY (version, need)
        );
        CREATE TABLE IF NOT EXISTS closures (
            version  TEXT NOT NULL,
            event_id TEXT NOT NULL,
            point_id TEXT NOT NULL,
            from_ts  INTEGER NOT NULL,
            to_ts    INTEGER NOT NULL,
            PRIMARY KEY (version, event_id)
        );
        CREATE TABLE IF NOT EXISTS capability_events (
            version    TEXT NOT NULL,
            event_id   TEXT NOT NULL,
            point_id   TEXT NOT NULL,
            capability TEXT NOT NULL,
            from_ts    INTEGER NOT NULL,
            to_ts      INTEGER NOT NULL,
            PRIMARY KEY (version, event_id)
        );
        CREATE TABLE IF NOT EXISTS home_service (
            version         TEXT PRIMARY KEY,
            allowed_service TEXT NOT NULL,
            reason          TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS home_service_mobility (
            version  TEXT NOT NULL,
            mobility TEXT NOT NULL,
            PRIMARY KEY (version, mobility)
        );
        CREATE TABLE IF NOT EXISTS home_service_slots (
            version  TEXT NOT NULL,
            slot_id  TEXT NOT NULL,
            from_ts  INTEGER NOT NULL,
            to_ts    INTEGER NOT NULL,
            capacity INTEGER NOT NULL,
            cost     INTEGER NOT NULL,
            PRIMARY KEY (version, slot_id)
        );
        CREATE TABLE IF NOT EXISTS bookings (
            booking_id  TEXT PRIMARY KEY,
            version     TEXT NOT NULL,
            slot_id     TEXT NOT NULL,
            snapshot_id TEXT NOT NULL,
            created_at  TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS snapshots (
            snapshot_id  TEXT PRIMARY KEY,
            version      TEXT NOT NULL,
            kind         TEXT NOT NULL,
            request_json TEXT NOT NULL,
            response_json TEXT NOT NULL,
            created_at   TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_closures_point ON closures(version, point_id);
        CREATE INDEX IF NOT EXISTS idx_snapshots_version ON snapshots(version);
        CREATE INDEX IF NOT EXISTS idx_bookings_slot ON bookings(version, slot_id);
        ",
    )?;
    Ok(())
}

/// Import a catalog version and atomically make it the active one.
/// The whole import is a single transaction: concurrent readers either see
/// the previous active version in full or the new one, never a mixture.
pub fn import_catalog(pool: &Pool, cat: &CatalogImport) -> Result<ImportResponse, DbError> {
    let mut conn = pool.get()?;
    let tx = conn.transaction()?;

    let exists: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM catalog_versions WHERE version = ?1",
            params![cat.catalog_version],
            |r| r.get(0),
        )
        .optional()?;
    if exists.is_some() {
        return Err(DbError::DuplicateVersion(cat.catalog_version.clone()));
    }

    tx.execute(
        "UPDATE catalog_versions SET is_active = 0 WHERE is_active = 1",
        [],
    )?;
    tx.execute(
        "INSERT INTO catalog_versions (version, cost_formula, imported_at, is_active)
         VALUES (?1, ?2, ?3, 1)",
        params![
            cat.catalog_version,
            cat.cost_formula,
            format_rfc3339(chrono::Utc::now().timestamp())
        ],
    )?;

    {
        let mut sp = tx.prepare(
            "INSERT INTO service_points (version, point_id, grid_x, grid_y, barrier_penalty)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let mut ps = tx.prepare(
            "INSERT INTO point_services (version, point_id, service) VALUES (?1, ?2, ?3)",
        )?;
        let mut pa = tx.prepare(
            "INSERT INTO point_access (version, point_id, access) VALUES (?1, ?2, ?3)",
        )?;
        for p in &cat.points {
            sp.execute(params![
                cat.catalog_version,
                p.id,
                p.grid[0],
                p.grid[1],
                p.barrier_penalty
            ])?;
            let mut services = p.services.clone();
            services.sort();
            services.dedup();
            for s in services {
                ps.execute(params![cat.catalog_version, p.id, s])?;
            }
            let mut access = p.access.clone();
            access.sort();
            access.dedup();
            for a in access {
                pa.execute(params![cat.catalog_version, p.id, a])?;
            }
        }
    }

    {
        let mut hr = tx.prepare(
            "INSERT INTO hard_requirements (version, need, access) VALUES (?1, ?2, ?3)",
        )?;
        for (need, access) in &cat.hard_requirements {
            hr.execute(params![cat.catalog_version, need, access])?;
        }
    }

    {
        let mut cl = tx.prepare(
            "INSERT INTO closures (version, event_id, point_id, from_ts, to_ts)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for c in &cat.closures {
            let from_ts = parse_rfc3339(&c.from)?;
            let to_ts = parse_rfc3339(&c.to)?;
            if from_ts >= to_ts {
                return Err(DbError::BadTime(format!(
                    "closure {} has from >= to",
                    c.event_id
                )));
            }
            cl.execute(params![cat.catalog_version, c.event_id, c.point_id, from_ts, to_ts])?;
        }
    }

    {
        let mut ce = tx.prepare(
            "INSERT INTO capability_events (version, event_id, point_id, capability, from_ts, to_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for e in &cat.capability_events {
            let from_ts = parse_rfc3339(&e.from)?;
            let to_ts = parse_rfc3339(&e.to)?;
            if from_ts >= to_ts {
                return Err(DbError::BadTime(format!(
                    "capability event {} has from >= to",
                    e.event_id
                )));
            }
            ce.execute(params![
                cat.catalog_version,
                e.event_id,
                e.point_id,
                e.capability,
                from_ts,
                to_ts
            ])?;
        }
    }

    if let Some(hs) = &cat.home_service {
        tx.execute(
            "INSERT INTO home_service (version, allowed_service, reason) VALUES (?1, ?2, ?3)",
            params![cat.catalog_version, hs.allowed_service, hs.reason],
        )?;
        let mut m = tx.prepare(
            "INSERT INTO home_service_mobility (version, mobility) VALUES (?1, ?2)",
        )?;
        let mut mob = hs.allowed_mobility.clone();
        mob.sort();
        mob.dedup();
        for v in mob {
            m.execute(params![cat.catalog_version, v])?;
        }
        let mut sl = tx.prepare(
            "INSERT INTO home_service_slots (version, slot_id, from_ts, to_ts, capacity, cost)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for s in &hs.slots {
            let from_ts = parse_rfc3339(&s.from)?;
            let to_ts = parse_rfc3339(&s.to)?;
            if from_ts >= to_ts {
                return Err(DbError::BadTime(format!(
                    "home service slot {} has from >= to",
                    s.slot_id
                )));
            }
            if s.capacity < 0 {
                return Err(DbError::BadTime(format!(
                    "home service slot {} has negative capacity",
                    s.slot_id
                )));
            }
            sl.execute(params![
                cat.catalog_version,
                s.slot_id,
                from_ts,
                to_ts,
                s.capacity,
                s.cost
            ])?;
        }
    }

    tx.commit()?;
    Ok(ImportResponse {
        version: cat.catalog_version.clone(),
        active: true,
        points: cat.points.len(),
        closures: cat.closures.len(),
        capability_events: cat.capability_events.len(),
    })
}

pub fn active_version(conn: &Connection) -> Result<Option<String>, DbError> {
    Ok(conn
        .query_row(
            "SELECT version FROM catalog_versions WHERE is_active = 1",
            [],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn active_catalog_info(conn: &Connection) -> Result<Option<ActiveCatalogResponse>, DbError> {
    let row = conn
        .query_row(
            "SELECT version, cost_formula, imported_at FROM catalog_versions WHERE is_active = 1",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    match row {
        None => Ok(None),
        Some((version, cost_formula, imported_at)) => {
            let points: i64 = conn.query_row(
                "SELECT COUNT(*) FROM service_points WHERE version = ?1",
                params![version],
                |r| r.get(0),
            )?;
            Ok(Some(ActiveCatalogResponse {
                version,
                cost_formula,
                points: points as usize,
                imported_at,
            }))
        }
    }
}

/// Load the full catalog for one version. Callers hold a read transaction so
/// the whole load is a consistent snapshot of that version.
pub fn load_catalog(conn: &Connection, version: &str) -> Result<Catalog, DbError> {
    let cost_formula: Option<String> = conn.query_row(
        "SELECT cost_formula FROM catalog_versions WHERE version = ?1",
        params![version],
        |r| r.get(0),
    )?;

    let mut points = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT point_id, grid_x, grid_y, barrier_penalty FROM service_points
             WHERE version = ?1 ORDER BY point_id",
        )?;
        let rows = stmt.query_map(params![version], |r| {
            Ok(Point {
                id: r.get(0)?,
                grid_x: r.get(1)?,
                grid_y: r.get(2)?,
                barrier_penalty: r.get(3)?,
                services: Vec::new(),
                access: Vec::new(),
            })
        })?;
        for p in rows {
            points.push(p?);
        }
    }
    {
        let mut stmt = conn.prepare(
            "SELECT point_id, service FROM point_services WHERE version = ?1
             ORDER BY point_id, service",
        )?;
        let rows = stmt.query_map(params![version], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let rows: Vec<(String, String)> = rows.collect::<Result<_, _>>()?;
        attach(&mut points, rows, |p, s| p.services.push(s));
    }
    {
        let mut stmt = conn.prepare(
            "SELECT point_id, access FROM point_access WHERE version = ?1
             ORDER BY point_id, access",
        )?;
        let rows = stmt.query_map(params![version], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let rows: Vec<(String, String)> = rows.collect::<Result<_, _>>()?;
        attach(&mut points, rows, |p, a| p.access.push(a));
    }

    let mut hard_requirements = BTreeMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT need, access FROM hard_requirements WHERE version = ?1 ORDER BY need",
        )?;
        let rows = stmt.query_map(params![version], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for r in rows {
            let (need, access) = r?;
            hard_requirements.insert(need, access);
        }
    }

    let mut closures = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT event_id, point_id, from_ts, to_ts FROM closures
             WHERE version = ?1 ORDER BY point_id, from_ts, event_id",
        )?;
        let rows = stmt.query_map(params![version], |r| {
            let from_ts: i64 = r.get(2)?;
            let to_ts: i64 = r.get(3)?;
            Ok(Closure {
                event_id: r.get(0)?,
                point_id: r.get(1)?,
                from_ts,
                to_ts,
                from_rfc3339: format_rfc3339(from_ts),
                to_rfc3339: format_rfc3339(to_ts),
            })
        })?;
        for c in rows {
            closures.push(c?);
        }
    }

    let mut capability_events = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT event_id, point_id, capability, from_ts, to_ts FROM capability_events
             WHERE version = ?1 ORDER BY point_id, from_ts, event_id",
        )?;
        let rows = stmt.query_map(params![version], |r| {
            let from_ts: i64 = r.get(3)?;
            let to_ts: i64 = r.get(4)?;
            Ok(CapabilityEvent {
                event_id: r.get(0)?,
                point_id: r.get(1)?,
                capability: r.get(2)?,
                from_ts,
                to_ts,
                from_rfc3339: format_rfc3339(from_ts),
                to_rfc3339: format_rfc3339(to_ts),
            })
        })?;
        for e in rows {
            capability_events.push(e?);
        }
    }

    let home_service: Option<(String, String)> = conn
        .query_row(
            "SELECT allowed_service, reason FROM home_service WHERE version = ?1",
            params![version],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let home_service = match home_service {
        None => None,
        Some((allowed_service, reason)) => {
            let mut stmt = conn.prepare(
                "SELECT mobility FROM home_service_mobility WHERE version = ?1 ORDER BY mobility",
            )?;
            let rows = stmt.query_map(params![version], |r| r.get::<_, String>(0))?;
            let mut allowed_mobility = Vec::new();
            for m in rows {
                allowed_mobility.push(m?);
            }
            let mut slots = Vec::new();
            {
                let mut stmt = conn.prepare(
                    "SELECT slot_id, from_ts, to_ts, capacity, cost FROM home_service_slots
                     WHERE version = ?1 ORDER BY cost, slot_id",
                )?;
                let rows = stmt.query_map(params![version], |r| {
                    let from_ts: i64 = r.get(1)?;
                    let to_ts: i64 = r.get(2)?;
                    Ok(HomeServiceSlot {
                        slot_id: r.get(0)?,
                        from_ts,
                        to_ts,
                        capacity: r.get(3)?,
                        cost: r.get(4)?,
                        from_rfc3339: format_rfc3339(from_ts),
                        to_rfc3339: format_rfc3339(to_ts),
                    })
                })?;
                for s in rows {
                    slots.push(s?);
                }
            }
            Some(HomeService {
                allowed_service,
                allowed_mobility,
                reason,
                slots,
            })
        }
    };

    Ok(Catalog {
        version: version.to_string(),
        cost_formula,
        points,
        hard_requirements,
        closures,
        capability_events,
        home_service,
    })
}

fn attach(points: &mut [Point], rows: Vec<(String, String)>, mut f: impl FnMut(&mut Point, String)) {
    for (pid, v) in rows {
        if let Some(p) = points.iter_mut().find(|p| p.id == pid) {
            f(p, v);
        }
    }
}

pub fn save_snapshot(
    conn: &Connection,
    snapshot_id: &str,
    version: &str,
    kind: &str,
    request_json: &str,
    response_json: &str,
) -> Result<(), DbError> {
    conn.execute(
        "INSERT INTO snapshots (snapshot_id, version, kind, request_json, response_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            snapshot_id,
            version,
            kind,
            request_json,
            response_json,
            format_rfc3339(chrono::Utc::now().timestamp())
        ],
    )?;
    Ok(())
}

/// Count existing bookings for one slot of one catalog version. Callers run
/// this inside an IMMEDIATE transaction so the check-and-book is atomic.
pub fn count_bookings(conn: &Connection, version: &str, slot_id: &str) -> Result<i64, DbError> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM bookings WHERE version = ?1 AND slot_id = ?2",
        params![version, slot_id],
        |r| r.get(0),
    )?)
}

pub fn insert_booking(
    conn: &Connection,
    booking_id: &str,
    version: &str,
    slot_id: &str,
    snapshot_id: &str,
) -> Result<(), DbError> {
    conn.execute(
        "INSERT INTO bookings (booking_id, version, slot_id, snapshot_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            booking_id,
            version,
            slot_id,
            snapshot_id,
            format_rfc3339(chrono::Utc::now().timestamp())
        ],
    )?;
    Ok(())
}

/// Fetch a stored snapshot response verbatim. Replays never recompute, so an
/// old snapshot always reflects exactly the catalog version it was built from.
pub fn load_snapshot(conn: &Connection, snapshot_id: &str) -> Result<Option<String>, DbError> {
    Ok(conn
        .query_row(
            "SELECT response_json FROM snapshots WHERE snapshot_id = ?1",
            params![snapshot_id],
            |r| r.get(0),
        )
        .optional()?)
}
