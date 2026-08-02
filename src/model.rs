//! Serializable request/response and catalog types.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Default mobility when omitted: a standard applicant with no extra access need.
pub const STANDARD_MOBILITY: &str = "STANDARD";

// ---------------------------------------------------------------------------
// Catalog import payload (mirrors materials/service-catalog.json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct CatalogImport {
    #[serde(rename = "catalogVersion")]
    pub catalog_version: String,
    #[serde(rename = "costFormula", default)]
    pub cost_formula: Option<String>,
    pub points: Vec<PointImport>,
    #[serde(rename = "hardRequirements", default)]
    pub hard_requirements: BTreeMap<String, String>,
    #[serde(rename = "tieBreak", default)]
    pub tie_break: Option<Vec<String>>,
    #[serde(default)]
    pub closures: Vec<ClosureImport>,
    #[serde(rename = "capabilityEvents", default)]
    pub capability_events: Vec<CapabilityEventImport>,
    #[serde(rename = "homeService", default)]
    pub home_service: Option<HomeServiceImport>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PointImport {
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
pub struct ClosureImport {
    #[serde(rename = "eventId")]
    pub event_id: String,
    #[serde(rename = "pointId")]
    pub point_id: String,
    pub from: String,
    pub to: String,
}

/// A capability degradation: one access capability at one point is
/// temporarily unavailable on the half-open interval [from, to).
#[derive(Debug, Clone, Deserialize)]
pub struct CapabilityEventImport {
    #[serde(rename = "eventId")]
    pub event_id: String,
    #[serde(rename = "pointId")]
    pub point_id: String,
    pub capability: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HomeServiceImport {
    #[serde(rename = "allowedService")]
    pub allowed_service: String,
    #[serde(rename = "allowedMobility", default)]
    pub allowed_mobility: Vec<String>,
    pub reason: String,
    #[serde(default)]
    pub slots: Vec<HomeServiceSlotImport>,
}

/// A bookable home-service appointment window with a capacity limit.
#[derive(Debug, Clone, Deserialize)]
pub struct HomeServiceSlotImport {
    #[serde(rename = "slotId")]
    pub slot_id: String,
    pub from: String,
    pub to: String,
    pub capacity: i64,
    #[serde(default)]
    pub cost: i64,
}

// ---------------------------------------------------------------------------
// In-memory catalog loaded from SQLite for exactly one version
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Catalog {
    pub version: String,
    pub cost_formula: Option<String>,
    pub points: Vec<Point>, // sorted by point id
    /// need (mobility or communication) -> required access capability
    pub hard_requirements: BTreeMap<String, String>,
    pub closures: Vec<Closure>, // sorted by (point id, from, event id)
    pub capability_events: Vec<CapabilityEvent>, // sorted by (point id, from, event id)
    pub home_service: Option<HomeService>,
}

#[derive(Debug, Clone)]
pub struct Point {
    pub id: String,
    pub grid_x: i64,
    pub grid_y: i64,
    pub barrier_penalty: i64,
    pub services: Vec<String>, // sorted, deduped
    pub access: Vec<String>,   // sorted, deduped
}

#[derive(Debug, Clone)]
pub struct Closure {
    pub event_id: String,
    pub point_id: String,
    pub from_ts: i64, // epoch seconds, inclusive
    pub to_ts: i64,   // epoch seconds, exclusive
    pub from_rfc3339: String,
    pub to_rfc3339: String,
}

#[derive(Debug, Clone)]
pub struct CapabilityEvent {
    pub event_id: String,
    pub point_id: String,
    pub capability: String,
    pub from_ts: i64, // epoch seconds, inclusive
    pub to_ts: i64,   // epoch seconds, exclusive
    pub from_rfc3339: String,
    pub to_rfc3339: String,
}

#[derive(Debug, Clone)]
pub struct HomeService {
    pub allowed_service: String,
    pub allowed_mobility: Vec<String>, // sorted, deduped
    pub reason: String,
    pub slots: Vec<HomeServiceSlot>, // sorted by (cost, slot id)
}

#[derive(Debug, Clone)]
pub struct HomeServiceSlot {
    pub slot_id: String,
    pub from_ts: i64,
    pub to_ts: i64, // a slot is bookable while `at` < to_ts
    pub capacity: i64,
    pub cost: i64,
    pub from_rfc3339: String,
    pub to_rfc3339: String,
}

// ---------------------------------------------------------------------------
// Route request / response
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct RouteRequest {
    #[serde(rename = "serviceNeed")]
    pub service_need: String,
    #[serde(default = "default_mobility")]
    pub mobility: String,
    #[serde(default)]
    pub communication: Vec<String>,
    pub origin: GridOrigin,
    /// RFC3339 evaluation time; defaults to "now" when omitted.
    #[serde(default)]
    pub at: Option<String>,
}

fn default_mobility() -> String {
    STANDARD_MOBILITY.to_string()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct GridOrigin {
    pub x: i64,
    pub y: i64,
}

/// Normalized request echoed back inside the snapshot so that permuted
/// inputs (e.g. communication lists in any order) hash to the same snapshot.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NormalizedRequest {
    #[serde(rename = "serviceNeed")]
    pub service_need: String,
    pub mobility: String,
    pub communication: Vec<String>, // sorted, deduped
    pub origin: GridOrigin,
    pub at: String, // RFC3339, seconds precision
    #[serde(rename = "atEpoch")]
    pub at_epoch: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CostBreakdown {
    pub distance: i64,
    #[serde(rename = "barrierPenalty")]
    pub barrier_penalty: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Candidate {
    #[serde(rename = "pointId")]
    pub point_id: String,
    #[serde(rename = "totalCost")]
    pub total_cost: i64,
    #[serde(rename = "costBreakdown")]
    pub cost_breakdown: CostBreakdown,
    pub services: Vec<String>,
    pub access: Vec<String>,
}

/// One reason in an exclusion chain. `code` is stable; other fields optional.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ExclusionReason {
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
    #[serde(rename = "eventId", skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
}

impl ExclusionReason {
    pub fn simple(code: &str) -> Self {
        ExclusionReason {
            code: code.to_string(),
            capability: None,
            event_id: None,
            from: None,
            to: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Exclusion {
    #[serde(rename = "pointId")]
    pub point_id: String,
    pub reasons: Vec<ExclusionReason>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HomeServiceBooking {
    #[serde(rename = "bookingId")]
    pub booking_id: String,
    #[serde(rename = "slotId")]
    pub slot_id: String,
    pub from: String,
    pub to: String,
    pub cost: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HomeServiceOutcome {
    pub eligible: bool,
    pub reason: String,
    /// Present when capacity was atomically reserved for this request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub booking: Option<HomeServiceBooking>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RouteOutcome {
    #[serde(rename = "snapshotId")]
    pub snapshot_id: String,
    #[serde(rename = "catalogVersion")]
    pub catalog_version: String,
    #[serde(rename = "costFormula")]
    pub cost_formula: Option<String>,
    pub request: NormalizedRequest,
    pub candidates: Vec<Candidate>,
    pub exclusions: Vec<Exclusion>,
    #[serde(rename = "homeService")]
    pub home_service: Option<HomeServiceOutcome>,
}

// ---------------------------------------------------------------------------
// Misc API payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct BatchRouteRequest {
    pub requests: Vec<RouteRequest>,
}

#[derive(Debug, Serialize)]
pub struct BatchRouteResponse {
    #[serde(rename = "batchId")]
    pub batch_id: String,
    #[serde(rename = "catalogVersion")]
    pub catalog_version: String,
    pub snapshots: Vec<RouteOutcome>,
}

#[derive(Debug, Serialize)]
pub struct ImportResponse {
    pub version: String,
    pub active: bool,
    pub points: usize,
    pub closures: usize,
    #[serde(rename = "capabilityEvents")]
    pub capability_events: usize,
}

#[derive(Debug, Serialize)]
pub struct ActiveCatalogResponse {
    pub version: String,
    #[serde(rename = "costFormula")]
    pub cost_formula: Option<String>,
    pub points: usize,
    #[serde(rename = "importedAt")]
    pub imported_at: String,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub error: String,
}
