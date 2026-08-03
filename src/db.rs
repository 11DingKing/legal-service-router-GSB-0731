use crate::models::{Catalog, CatalogSummary, SnapshotRecord};
use chrono::Utc;
use rusqlite::{params, Connection};
use std::sync::Mutex;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

pub struct Db {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS catalogs (
    catalog_version TEXT PRIMARY KEY,
    payload TEXT NOT NULL,
    imported_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS snapshots (
    snapshot_id TEXT PRIMARY KEY,
    catalog_version TEXT NOT NULL,
    request_payload TEXT NOT NULL,
    result_payload TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS current_catalog (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    catalog_version TEXT NOT NULL
);
"#;

impl Db {
    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    pub fn open_file(path: &str) -> Result<Self, DbError> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self, DbError> {
        conn.execute_batch(SCHEMA)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")?;
        Ok(Db {
            conn: Mutex::new(conn),
        })
    }

    pub fn import_catalog(&self, catalog: &Catalog) -> Result<(), DbError> {
        let payload = serde_json::to_string(catalog)?;
        let now = Utc::now().to_rfc3339();
        let version = catalog.catalog_version.clone();
        let mut conn = self.conn.lock().expect("db lock poisoned");
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO catalogs (catalog_version, payload, imported_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(catalog_version) DO UPDATE SET payload=excluded.payload, imported_at=excluded.imported_at",
            params![version, payload, now],
        )?;
        tx.execute(
            "INSERT INTO current_catalog (id, catalog_version) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET catalog_version=excluded.catalog_version",
            params![version],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn current_catalog_version(&self) -> Result<Option<String>, DbError> {
        let conn = self.conn.lock().expect("db lock poisoned");
        let mut stmt = conn.prepare("SELECT catalog_version FROM current_catalog WHERE id = 1")?;
        let mut rows = stmt.query([])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    pub fn load_catalog(&self, version: &str) -> Result<Option<Catalog>, DbError> {
        let conn = self.conn.lock().expect("db lock poisoned");
        let mut stmt = conn.prepare("SELECT payload FROM catalogs WHERE catalog_version = ?1")?;
        let mut rows = stmt.query(params![version])?;
        if let Some(row) = rows.next()? {
            let payload: String = row.get(0)?;
            let catalog: Catalog = serde_json::from_str(&payload)?;
            Ok(Some(catalog))
        } else {
            Ok(None)
        }
    }

    pub fn load_current_catalog(&self) -> Result<Option<Catalog>, DbError> {
        match self.current_catalog_version()? {
            Some(v) => self.load_catalog(&v),
            None => Ok(None),
        }
    }

    pub fn save_snapshot(
        &self,
        snapshot_id: &str,
        catalog_version: &str,
        request_payload: &str,
        result_payload: &str,
    ) -> Result<(), DbError> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().expect("db lock poisoned");
        conn.execute(
            "INSERT INTO snapshots (snapshot_id, catalog_version, request_payload, result_payload, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![snapshot_id, catalog_version, request_payload, result_payload, now],
        )?;
        Ok(())
    }

    pub fn load_snapshot(&self, snapshot_id: &str) -> Result<Option<SnapshotRecord>, DbError> {
        let conn = self.conn.lock().expect("db lock poisoned");
        let mut stmt = conn.prepare(
            "SELECT snapshot_id, catalog_version, result_payload, created_at FROM snapshots WHERE snapshot_id = ?1",
        )?;
        let mut rows = stmt.query(params![snapshot_id])?;
        if let Some(row) = rows.next()? {
            let created_at_str: String = row.get(3)?;
            let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_str)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            Ok(Some(SnapshotRecord {
                id: row.get(0)?,
                catalog_version: row.get(1)?,
                payload: row.get(2)?,
                created_at,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn catalog_summary(&self) -> Result<Option<CatalogSummary>, DbError> {
        let catalog = self.load_current_catalog()?;
        match catalog {
            Some(c) => {
                let conn = self.conn.lock().expect("db lock poisoned");
                let imported_at_str: String = conn.query_row(
                    "SELECT imported_at FROM catalogs WHERE catalog_version = ?1",
                    params![c.catalog_version],
                    |row| row.get(0),
                )?;
                let imported_at = chrono::DateTime::parse_from_rfc3339(&imported_at_str)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                Ok(Some(CatalogSummary {
                    catalog_version: c.catalog_version,
                    point_count: c.points.len(),
                    closure_count: c.closures.len(),
                    imported_at,
                }))
            }
            None => Ok(None),
        }
    }
}
