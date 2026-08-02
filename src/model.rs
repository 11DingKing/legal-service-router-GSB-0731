//! Domain model and catalog (de)serialization.
//!
//! The on-disk fixture `materials/service-catalog.json` is the authoritative
//! shape for imports. [`RawCatalog`] mirrors that JSON exactly; [`Catalog`] is
//! the normalized, immutable in-memory form used for routing. Normalization
//! sorts every collection so that a catalog is independent of the input order
//! in which points / capabilities were listed.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Manhattan grid coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grid {
    pub x: i64,
    pub y: i64,
}

impl Grid {
    pub fn manhattan(&self, other: &Grid) -> i64 {
        (self.x - other.x).abs() + (self.y - other.y).abs()
    }
}

/// Raw catalog exactly as stored in `service-catalog.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct RawCatalog {
    #[serde(rename = "catalogVersion")]
    pub catalog_version: String,
    #[serde(rename = "costFormula", default)]
    pub cost_formula: String,
    pub points: Vec<RawPoint>,
    /// Maps an applicant need key (e.g. `WHEELCHAIR`) to the access capability
    /// a point MUST provide (e.g. `STEP_FREE`).
    #[serde(rename = "hardRequirements", default)]
    pub hard_requirements: BTreeMap<String, String>,
    #[serde(rename = "tieBreak", default)]
    pub tie_break: Vec<String>,
    #[serde(default)]
    pub closures: Vec<RawClosure>,
    #[serde(rename = "homeService")]
    pub home_service: Option<RawHomeService>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawPoint {
    pub id: String,
    pub grid: [i64; 2],
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub access: Vec<String>,
    #[serde(rename = "barrierPenalty", default)]
    pub barrier_penalty: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawClosure {
    #[serde(rename = "eventId")]
    pub event_id: String,
    #[serde(rename = "pointId")]
    pub point_id: String,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawHomeService {
    #[serde(rename = "allowedService")]
    pub allowed_service: String,
    #[serde(rename = "allowedMobility", default)]
    pub allowed_mobility: Vec<String>,
    pub reason: String,
}

/// A normalized service point. All capability collections are sorted sets so
/// equality and iteration are order independent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Point {
    pub id: String,
    pub grid: Grid,
    pub services: BTreeSet<String>,
    pub access: BTreeSet<String>,
    pub barrier_penalty: i64,
}

/// A temporary closure with half-open interval semantics `[from, to)`:
/// the point is closed at `from` and open again exactly at `to`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Closure {
    pub event_id: String,
    pub point_id: String,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

impl Closure {
    /// True when the point is closed at instant `at` (half-open `[from, to)`).
    pub fn covers(&self, at: DateTime<Utc>) -> bool {
        at >= self.from && at < self.to
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HomeService {
    pub allowed_service: String,
    pub allowed_mobility: BTreeSet<String>,
    pub reason: String,
}

/// Immutable, normalized catalog. Shared as `Arc<Catalog>`; a single routing
/// request captures one `Arc` and therefore observes exactly one complete
/// version even while a hot reload swaps in a newer one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Catalog {
    pub version: String,
    pub cost_formula: String,
    /// Points keyed by id -> deterministic iteration order.
    pub points: BTreeMap<String, Point>,
    pub hard_requirements: BTreeMap<String, String>,
    pub tie_break: Vec<String>,
    /// Closures keyed by point id for quick lookup.
    pub closures_by_point: BTreeMap<String, Vec<Closure>>,
    pub home_service: Option<HomeService>,
}

impl Catalog {
    /// Build a normalized catalog from the raw JSON form.
    pub fn from_raw(raw: RawCatalog) -> Result<Catalog, String> {
        let mut points = BTreeMap::new();
        for p in raw.points {
            if points.contains_key(&p.id) {
                return Err(format!("duplicate point id: {}", p.id));
            }
            points.insert(
                p.id.clone(),
                Point {
                    id: p.id,
                    grid: Grid { x: p.grid[0], y: p.grid[1] },
                    services: p.services.into_iter().collect(),
                    access: p.access.into_iter().collect(),
                    barrier_penalty: p.barrier_penalty,
                },
            );
        }

        let mut closures_by_point: BTreeMap<String, Vec<Closure>> = BTreeMap::new();
        for c in raw.closures {
            if c.to < c.from {
                return Err(format!("closure {} has to < from", c.event_id));
            }
            closures_by_point
                .entry(c.point_id.clone())
                .or_default()
                .push(Closure {
                    event_id: c.event_id,
                    point_id: c.point_id,
                    from: c.from,
                    to: c.to,
                });
        }
        // Sort each point's closures by event id for stable output.
        for list in closures_by_point.values_mut() {
            list.sort_by(|a, b| a.event_id.cmp(&b.event_id));
        }

        let home_service = raw.home_service.map(|h| HomeService {
            allowed_service: h.allowed_service,
            allowed_mobility: h.allowed_mobility.into_iter().collect(),
            reason: h.reason,
        });

        Ok(Catalog {
            version: raw.catalog_version,
            cost_formula: raw.cost_formula,
            points,
            hard_requirements: raw.hard_requirements,
            tie_break: raw.tie_break,
            closures_by_point,
            home_service,
        })
    }

    /// Parse a catalog directly from JSON bytes.
    pub fn from_json(bytes: &[u8]) -> Result<Catalog, String> {
        let raw: RawCatalog =
            serde_json::from_slice(bytes).map_err(|e| format!("invalid catalog json: {e}"))?;
        Catalog::from_raw(raw)
    }
}
