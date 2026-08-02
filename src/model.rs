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
    /// Temporary capability-degradation events that partially overlap closures.
    #[serde(default)]
    pub degradations: Vec<RawDegradation>,
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
pub struct RawDegradation {
    #[serde(rename = "eventId")]
    pub event_id: String,
    #[serde(rename = "pointId")]
    pub point_id: String,
    /// The single access capability temporarily removed while active.
    pub capability: String,
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
    /// Bookable home-visit time slots with finite appointment capacity.
    #[serde(default)]
    pub slots: Vec<RawHomeSlot>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawHomeSlot {
    #[serde(rename = "slotId")]
    pub slot_id: String,
    pub cost: i64,
    pub capacity: i64,
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

/// A temporary capability-degradation event. While active over half-open
/// `[from, to)`, the point loses `capability` (e.g. its lift is broken, so
/// `STEP_FREE` is temporarily unavailable) without the whole point closing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Degradation {
    pub event_id: String,
    pub point_id: String,
    pub capability: String,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

impl Degradation {
    /// True when the degradation is active at instant `at` (half-open).
    pub fn covers(&self, at: DateTime<Utc>) -> bool {
        at >= self.from && at < self.to
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HomeService {
    pub allowed_service: String,
    pub allowed_mobility: BTreeSet<String>,
    pub reason: String,
    /// Bookable slots, ordered by the same tie-break as physical candidates:
    /// cost ascending, then slot id ascending. Deterministic regardless of the
    /// order slots were declared in the catalog.
    pub slots: Vec<HomeSlot>,
}

/// A home-visit appointment slot definition (immutable). Live remaining
/// capacity is tracked separately in the store, not in the catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HomeSlot {
    pub slot_id: String,
    pub cost: i64,
    pub capacity: i64,
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
    /// Capability degradations keyed by point id.
    pub degradations_by_point: BTreeMap<String, Vec<Degradation>>,
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

        let mut degradations_by_point: BTreeMap<String, Vec<Degradation>> = BTreeMap::new();
        for d in raw.degradations {
            if d.to < d.from {
                return Err(format!("degradation {} has to < from", d.event_id));
            }
            degradations_by_point
                .entry(d.point_id.clone())
                .or_default()
                .push(Degradation {
                    event_id: d.event_id,
                    point_id: d.point_id,
                    capability: d.capability,
                    from: d.from,
                    to: d.to,
                });
        }
        for list in degradations_by_point.values_mut() {
            list.sort_by(|a, b| a.event_id.cmp(&b.event_id));
        }

        let home_service = raw.home_service.map(|h| {
            let mut slots: Vec<HomeSlot> = h
                .slots
                .into_iter()
                .map(|s| HomeSlot { slot_id: s.slot_id, cost: s.cost, capacity: s.capacity })
                .collect();
            // Deterministic order: cost ascending, then slot id ascending —
            // the same tie-break rule physical candidates use.
            slots.sort_by(|a, b| a.cost.cmp(&b.cost).then_with(|| a.slot_id.cmp(&b.slot_id)));
            HomeService {
                allowed_service: h.allowed_service,
                allowed_mobility: h.allowed_mobility.into_iter().collect(),
                reason: h.reason,
                slots,
            }
        });

        Ok(Catalog {
            version: raw.catalog_version,
            cost_formula: raw.cost_formula,
            points,
            hard_requirements: raw.hard_requirements,
            tie_break: raw.tie_break,
            closures_by_point,
            degradations_by_point,
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
