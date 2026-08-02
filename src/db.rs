use std::collections::HashMap;

use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::params;
use rusqlite::TransactionBehavior;
use uuid::Uuid;

use crate::catalog::{Catalog, Closure, HomeServiceConfig, Point};
use crate::error::{AppError, AppResult};
use crate::routing::{Candidate, Excluded, RouteRequest, RouteResult};

pub type DbPool = Pool<SqliteConnectionManager>;

pub fn init_pool(database_url: &str) -> AppResult<DbPool> {
    {
        let conn = rusqlite::Connection::open(database_url)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    }

    let manager = SqliteConnectionManager::file(database_url).with_init(|conn| {
        conn.execute_batch(
            "PRAGMA busy_timeout=15000;
             PRAGMA foreign_keys=ON;",
        )?;
        Ok(())
    });
    let pool = Pool::builder()
        .max_size(16)
        .build(manager)
        .map_err(AppError::Pool)?;

    init_schema(&pool)?;
    Ok(pool)
}

pub fn init_schema(pool: &DbPool) -> AppResult<()> {
    let conn = pool.get()?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS catalog_versions (
            version TEXT PRIMARY KEY,
            cost_formula TEXT NOT NULL,
            tie_break TEXT NOT NULL,
            imported_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS service_points (
            catalog_version TEXT NOT NULL,
            point_id TEXT NOT NULL,
            grid_x INTEGER NOT NULL,
            grid_y INTEGER NOT NULL,
            barrier_penalty INTEGER NOT NULL,
            PRIMARY KEY (catalog_version, point_id),
            FOREIGN KEY (catalog_version) REFERENCES catalog_versions(version)
        );

        CREATE TABLE IF NOT EXISTS point_services (
            catalog_version TEXT NOT NULL,
            point_id TEXT NOT NULL,
            service_code TEXT NOT NULL,
            PRIMARY KEY (catalog_version, point_id, service_code),
            FOREIGN KEY (catalog_version, point_id)
                REFERENCES service_points(catalog_version, point_id)
        );

        CREATE TABLE IF NOT EXISTS point_access (
            catalog_version TEXT NOT NULL,
            point_id TEXT NOT NULL,
            access_code TEXT NOT NULL,
            PRIMARY KEY (catalog_version, point_id, access_code),
            FOREIGN KEY (catalog_version, point_id)
                REFERENCES service_points(catalog_version, point_id)
        );

        CREATE TABLE IF NOT EXISTS closures (
            catalog_version TEXT NOT NULL,
            event_id TEXT NOT NULL,
            point_id TEXT NOT NULL,
            from_time TEXT NOT NULL,
            to_time TEXT NOT NULL,
            PRIMARY KEY (catalog_version, event_id),
            FOREIGN KEY (catalog_version, point_id)
                REFERENCES service_points(catalog_version, point_id)
        );

        CREATE TABLE IF NOT EXISTS hard_requirements (
            catalog_version TEXT NOT NULL,
            need_code TEXT NOT NULL,
            access_code TEXT NOT NULL,
            PRIMARY KEY (catalog_version, need_code),
            FOREIGN KEY (catalog_version) REFERENCES catalog_versions(version)
        );

        CREATE TABLE IF NOT EXISTS home_service_config (
            catalog_version TEXT PRIMARY KEY,
            allowed_service TEXT NOT NULL,
            allowed_mobility TEXT NOT NULL,
            reason TEXT NOT NULL,
            FOREIGN KEY (catalog_version) REFERENCES catalog_versions(version)
        );

        CREATE TABLE IF NOT EXISTS active_catalog (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            version TEXT NOT NULL,
            activated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS routing_snapshots (
            snapshot_id TEXT PRIMARY KEY,
            catalog_version TEXT NOT NULL,
            request_json TEXT NOT NULL,
            query_time TEXT NOT NULL,
            home_service_eligible INTEGER NOT NULL DEFAULT 0,
            home_service_reason TEXT,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS snapshot_candidates (
            snapshot_id TEXT NOT NULL,
            rank INTEGER NOT NULL,
            point_id TEXT NOT NULL,
            total_cost INTEGER NOT NULL,
            distance_cost INTEGER NOT NULL,
            barrier_penalty INTEGER NOT NULL,
            grid_x INTEGER NOT NULL,
            grid_y INTEGER NOT NULL,
            services_json TEXT NOT NULL,
            access_json TEXT NOT NULL,
            PRIMARY KEY (snapshot_id, rank),
            FOREIGN KEY (snapshot_id) REFERENCES routing_snapshots(snapshot_id)
        );

        CREATE TABLE IF NOT EXISTS snapshot_excluded (
            snapshot_id TEXT NOT NULL,
            point_id TEXT NOT NULL,
            reasons_json TEXT NOT NULL,
            sort_order INTEGER NOT NULL,
            PRIMARY KEY (snapshot_id, point_id)
        );

        CREATE INDEX IF NOT EXISTS idx_points_version
            ON service_points(catalog_version);
        CREATE INDEX IF NOT EXISTS idx_services_lookup
            ON point_services(catalog_version, service_code);
        CREATE INDEX IF NOT EXISTS idx_access_lookup
            ON point_access(catalog_version, access_code);
        CREATE INDEX IF NOT EXISTS idx_snapshot_version
            ON routing_snapshots(catalog_version);
        "#,
    )?;
    Ok(())
}

pub fn import_catalog(pool: &DbPool, catalog: &Catalog) -> AppResult<()> {
    let mut conn = pool.get()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let imported_at = Utc::now().to_rfc3339();

    let exists: bool = tx
        .prepare("SELECT 1 FROM catalog_versions WHERE version = ?1")?
        .exists(params![catalog.catalog_version])?;
    if exists {
        return Err(AppError::Conflict(format!(
            "catalog version '{}' already exists",
            catalog.catalog_version
        )));
    }

    tx.execute(
        "INSERT INTO catalog_versions (version, cost_formula, tie_break, imported_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            catalog.catalog_version,
            catalog.cost_formula,
            serde_json::to_string(&catalog.tie_break)?,
            imported_at,
        ],
    )?;

    for point in &catalog.points {
        tx.execute(
            "INSERT INTO service_points
                (catalog_version, point_id, grid_x, grid_y, barrier_penalty)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                catalog.catalog_version,
                point.id,
                point.grid[0],
                point.grid[1],
                point.barrier_penalty,
            ],
        )?;

        for service in &point.services {
            tx.execute(
                "INSERT INTO point_services (catalog_version, point_id, service_code)
                 VALUES (?1, ?2, ?3)",
                params![catalog.catalog_version, point.id, service],
            )?;
        }

        for access in &point.access {
            tx.execute(
                "INSERT INTO point_access (catalog_version, point_id, access_code)
                 VALUES (?1, ?2, ?3)",
                params![catalog.catalog_version, point.id, access],
            )?;
        }
    }

    for closure in &catalog.closures {
        tx.execute(
            "INSERT INTO closures
                (catalog_version, event_id, point_id, from_time, to_time)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                catalog.catalog_version,
                closure.event_id,
                closure.point_id,
                closure.from.to_rfc3339(),
                closure.to.to_rfc3339(),
            ],
        )?;
    }

    for (need_code, access_code) in &catalog.hard_requirements {
        tx.execute(
            "INSERT INTO hard_requirements (catalog_version, need_code, access_code)
             VALUES (?1, ?2, ?3)",
            params![catalog.catalog_version, need_code, access_code],
        )?;
    }

    tx.execute(
        "INSERT INTO home_service_config
            (catalog_version, allowed_service, allowed_mobility, reason)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            catalog.catalog_version,
            catalog.home_service.allowed_service,
            serde_json::to_string(&catalog.home_service.allowed_mobility)?,
            catalog.home_service.reason,
        ],
    )?;

    tx.execute(
        "INSERT INTO active_catalog (id, version, activated_at)
         VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET version = excluded.version,
                                       activated_at = excluded.activated_at",
        params![catalog.catalog_version, imported_at],
    )?;

    tx.commit()?;
    Ok(())
}

pub fn get_active_version(pool: &DbPool) -> AppResult<Option<String>> {
    let conn = pool.get()?;
    let version: Option<String> = conn
        .prepare("SELECT version FROM active_catalog WHERE id = 1")?
        .query_row([], |row| row.get(0))
        .ok();
    Ok(version)
}

pub fn version_exists(pool: &DbPool, version: &str) -> AppResult<bool> {
    let conn = pool.get()?;
    let exists = conn
        .prepare("SELECT 1 FROM catalog_versions WHERE version = ?1")?
        .exists(params![version])?;
    Ok(exists)
}

pub fn load_catalog(pool: &DbPool, version: &str) -> AppResult<Catalog> {
    let conn = pool.get()?;

    let (cost_formula, tie_break_str): (String, String) = conn
        .prepare(
            "SELECT cost_formula, tie_break FROM catalog_versions WHERE version = ?1",
        )?
        .query_row(params![version], |row| Ok((row.get(0)?, row.get(1)?)))?;

    let tie_break: Vec<String> = serde_json::from_str(&tie_break_str)?;

    let mut point_stmt = conn.prepare(
        "SELECT point_id, grid_x, grid_y, barrier_penalty
         FROM service_points
         WHERE catalog_version = ?1
         ORDER BY point_id ASC",
    )?;
    let point_rows = point_stmt.query_map(params![version], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i32>(1)?,
            row.get::<_, i32>(2)?,
            row.get::<_, i32>(3)?,
        ))
    })?;

    let mut point_order: Vec<String> = Vec::new();
    let mut points_map: HashMap<String, Point> = HashMap::new();
    for row in point_rows {
        let (id, gx, gy, bp) = row?;
        point_order.push(id.clone());
        points_map.insert(
            id,
            Point {
                id: String::new(),
                grid: [gx, gy],
                services: Vec::new(),
                access: Vec::new(),
                barrier_penalty: bp,
            },
        );
    }

    let mut svc_stmt = conn.prepare(
        "SELECT point_id, service_code FROM point_services
         WHERE catalog_version = ?1
         ORDER BY point_id ASC, service_code ASC",
    )?;
    let svc_rows = svc_stmt.query_map(params![version], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in svc_rows {
        let (point_id, service) = row?;
        if let Some(p) = points_map.get_mut(&point_id) {
            p.services.push(service);
        }
    }

    let mut acc_stmt = conn.prepare(
        "SELECT point_id, access_code FROM point_access
         WHERE catalog_version = ?1
         ORDER BY point_id ASC, access_code ASC",
    )?;
    let acc_rows = acc_stmt.query_map(params![version], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in acc_rows {
        let (point_id, access) = row?;
        if let Some(p) = points_map.get_mut(&point_id) {
            p.access.push(access);
        }
    }

    let mut points = Vec::with_capacity(point_order.len());
    for id in point_order {
        let mut p = points_map.remove(&id).expect("point in map");
        p.id = id;
        points.push(p);
    }

    let mut cl_stmt = conn.prepare(
        "SELECT event_id, point_id, from_time, to_time
         FROM closures
         WHERE catalog_version = ?1
         ORDER BY event_id ASC",
    )?;
    let closures: Vec<Closure> = cl_stmt
        .query_map(params![version], |row| {
            let from_s: String = row.get(2)?;
            let to_s: String = row.get(3)?;
            Ok(Closure {
                event_id: row.get(0)?,
                point_id: row.get(1)?,
                from: DateTime::parse_from_rfc3339(&from_s)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    ))?,
                to: DateTime::parse_from_rfc3339(&to_s)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    ))?,
            })
        })?
        .collect::<Result<_, _>>()?;

    let mut hr_stmt = conn.prepare(
        "SELECT need_code, access_code FROM hard_requirements
         WHERE catalog_version = ?1
         ORDER BY need_code ASC",
    )?;
    let hard_requirements: HashMap<String, String> = hr_stmt
        .query_map(params![version], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<_, _>>()?;

    let (allowed_service, allowed_mobility_str, reason): (String, String, String) = conn
        .prepare(
            "SELECT allowed_service, allowed_mobility, reason
             FROM home_service_config
             WHERE catalog_version = ?1",
        )?
        .query_row(params![version], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;

    let allowed_mobility: Vec<String> = serde_json::from_str(&allowed_mobility_str)?;

    Ok(Catalog {
        catalog_version: version.to_string(),
        cost_formula,
        points,
        hard_requirements,
        tie_break,
        closures,
        home_service: HomeServiceConfig {
            allowed_service,
            allowed_mobility,
            reason,
        },
    })
}

pub fn save_snapshot(
    pool: &DbPool,
    request: &RouteRequest,
    catalog_version: &str,
    query_time: &DateTime<Utc>,
    result: &RouteResult,
) -> AppResult<String> {
    let mut conn = pool.get()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let snapshot_id = Uuid::new_v4().to_string();
    let created_at = Utc::now().to_rfc3339();

    tx.execute(
        "INSERT INTO routing_snapshots
            (snapshot_id, catalog_version, request_json, query_time,
             home_service_eligible, home_service_reason, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            snapshot_id,
            catalog_version,
            serde_json::to_string(request)?,
            query_time.to_rfc3339(),
            result.home_service.as_ref().map_or(0, |_| 1),
            result.home_service.as_ref().map(|h| h.reason.clone()),
            created_at,
        ],
    )?;

    for c in &result.candidates {
        tx.execute(
            "INSERT INTO snapshot_candidates
                (snapshot_id, rank, point_id, total_cost, distance_cost,
                 barrier_penalty, grid_x, grid_y, services_json, access_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                snapshot_id,
                c.rank,
                c.point_id,
                c.total_cost,
                c.cost.distance,
                c.cost.barrier_penalty,
                c.grid[0],
                c.grid[1],
                serde_json::to_string(&c.services)?,
                serde_json::to_string(&c.access)?,
            ],
        )?;
    }

    for (idx, e) in result.excluded.iter().enumerate() {
        tx.execute(
            "INSERT INTO snapshot_excluded
                (snapshot_id, point_id, reasons_json, sort_order)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                snapshot_id,
                e.point_id,
                serde_json::to_string(&e.reasons)?,
                idx as i64,
            ],
        )?;
    }

    tx.commit()?;
    Ok(snapshot_id)
}

pub fn load_snapshot(
    pool: &DbPool,
    snapshot_id: &str,
) -> AppResult<(RouteRequest, String, DateTime<Utc>, RouteResult)> {
    let conn = pool.get()?;

    let row = conn
        .prepare(
            "SELECT request_json, catalog_version, query_time,
                    home_service_eligible, home_service_reason
             FROM routing_snapshots WHERE snapshot_id = ?1",
        )?
        .query_row(params![snapshot_id], |row| {
            let req_s: String = row.get(0)?;
            let cv: String = row.get(1)?;
            let qt_s: String = row.get(2)?;
            let hse: i32 = row.get(3)?;
            let hsr: Option<String> = row.get(4)?;
            Ok((req_s, cv, qt_s, hse, hsr))
        })
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => {
                AppError::NotFound(format!("snapshot '{snapshot_id}' not found"))
            }
            other => AppError::Db(other),
        })?;

    let (req_s, catalog_version, qt_s, hse, hsr) = row;
    let request: RouteRequest = serde_json::from_str(&req_s)?;
    let query_time = DateTime::parse_from_rfc3339(&qt_s)?.with_timezone(&Utc);

    let mut stmt = conn.prepare(
        "SELECT rank, point_id, total_cost, distance_cost, barrier_penalty,
                grid_x, grid_y, services_json, access_json
         FROM snapshot_candidates
         WHERE snapshot_id = ?1
         ORDER BY rank ASC",
    )?;
    let candidates: Vec<Candidate> = stmt
        .query_map(params![snapshot_id], |row| {
            let svc_s: String = row.get(7)?;
            let acc_s: String = row.get(8)?;
            Ok(Candidate {
                rank: row.get(0)?,
                point_id: row.get(1)?,
                total_cost: row.get(2)?,
                cost: crate::routing::CostBreakdown {
                    distance: row.get(3)?,
                    barrier_penalty: row.get(4)?,
                },
                grid: [row.get(5)?, row.get(6)?],
                services: serde_json::from_str(&svc_s).unwrap_or_default(),
                access: serde_json::from_str(&acc_s).unwrap_or_default(),
            })
        })?
        .collect::<Result<_, _>>()?;

    let mut stmt = conn.prepare(
        "SELECT point_id, reasons_json
         FROM snapshot_excluded
         WHERE snapshot_id = ?1
         ORDER BY sort_order ASC",
    )?;
    let excluded: Vec<Excluded> = stmt
        .query_map(params![snapshot_id], |row| {
            let reasons_s: String = row.get(1)?;
            Ok(Excluded {
                point_id: row.get(0)?,
                reasons: serde_json::from_str(&reasons_s).unwrap_or_default(),
            })
        })?
        .collect::<Result<_, _>>()?;

    let home_service = if hse != 0 {
        Some(crate::routing::HomeServiceResult {
            eligible: true,
            reason: hsr.unwrap_or_else(|| "HOME_SERVICE_REQUIRED".to_string()),
        })
    } else {
        None
    };

    let result = RouteResult {
        candidates,
        excluded,
        home_service,
        tie_break: Vec::new(),
    };

    Ok((request, catalog_version, query_time, result))
}

pub fn get_tie_break(pool: &DbPool, version: &str) -> AppResult<Vec<String>> {
    let conn = pool.get()?;
    let s: String = conn
        .prepare("SELECT tie_break FROM catalog_versions WHERE version = ?1")?
        .query_row(params![version], |row| row.get(0))?;
    Ok(serde_json::from_str(&s)?)
}
