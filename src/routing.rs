//! Constraint-based routing.
//!
//! Routing is a strict two-phase process:
//!
//! 1. **Hard filtering** — a candidate point is kept only if it (a) offers the
//!    requested service category, (b) provides every mandatory accessibility
//!    capability implied by the applicant's mobility / communication needs, and
//!    (c) is not temporarily closed at the query instant. A point that fails
//!    any hard check is *excluded with a reason* and can never be resurrected
//!    by being geographically closer.
//! 2. **Cost comparison** — only the survivors are scored with the catalog's
//!    deterministic Manhattan-plus-barrier cost, then sorted by the documented
//!    tie-break `(totalCost ascending, point id ascending)`.
//!
//! Every collection consumed here is order independent (sets / id-keyed maps),
//! so re-ordering the applicant's needs or the catalog's points yields byte
//! identical candidates, exclusions, and cost breakdowns.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::Catalog;

/// A routing request. `at` defaults to "now" when omitted so closures can be
/// evaluated deterministically against a caller-supplied instant in tests.
#[derive(Debug, Clone, Deserialize)]
pub struct RouteRequest {
    /// Applicant origin on the same grid as the points.
    pub origin: [i64; 2],
    /// Required service category, e.g. `LEGAL_AID`.
    pub service: String,
    /// Mobility need key, e.g. `WHEELCHAIR` or `HOMEBOUND`. Optional.
    #[serde(default)]
    pub mobility: Option<String>,
    /// Communication need keys, e.g. `HEARING`, `SPEECH`. Order independent.
    #[serde(default)]
    pub communication: Vec<String>,
    /// Instant at which closures are evaluated. Defaults to request time.
    #[serde(default)]
    pub at: Option<DateTime<Utc>>,
}

/// Deterministic breakdown of a candidate's reachability cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostBreakdown {
    pub manhattan: i64,
    pub barrier_penalty: i64,
    pub total: i64,
}

/// A point that survived hard filtering, with its cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub point_id: String,
    pub grid: [i64; 2],
    pub cost: CostBreakdown,
}

/// A point that failed a hard check, with the machine-readable reason chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exclusion {
    pub point_id: String,
    /// Ordered reason codes, e.g. `["MISSING_SERVICE", "MISSING_ACCESS:STEP_FREE"]`.
    pub reasons: Vec<String>,
}

/// Full routing outcome bound to a catalog version. Persisted verbatim as an
/// immutable snapshot; replay returns exactly this document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteResult {
    pub catalog_version: String,
    pub origin: [i64; 2],
    pub service: String,
    pub mobility: Option<String>,
    /// Normalized (sorted) communication needs actually evaluated.
    pub communication: Vec<String>,
    pub evaluated_at: DateTime<Utc>,
    pub candidates: Vec<Candidate>,
    pub exclusions: Vec<Exclusion>,
    /// Set only when the applicant qualifies for the home-service fallback (see
    /// [`route`]). The exclusion chain above is always preserved alongside it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home_service: Option<HomeServiceOutcome>,
}

/// Status of the home-service fallback for an eligible applicant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HomeServiceStatus {
    /// Eligible for the home fallback; appointment capacity not yet resolved.
    /// This is the state produced by the pure [`route`] function; the store
    /// resolves it to `Reserved` or `NoCapacity`.
    Eligible,
    /// A concrete appointment slot was reserved.
    Reserved,
    /// Eligible, but every home-visit slot is already at capacity. The result
    /// still lists zero physical candidates — capacity shortage never falls
    /// back to an inaccessible/closed physical point.
    NoCapacity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeServiceOutcome {
    /// The catalog-defined reason code, e.g. `HOME_SERVICE_REQUIRED`.
    pub reason: String,
    pub status: HomeServiceStatus,
    /// The reserved slot id (present only when `status == Reserved`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot_id: Option<String>,
    /// The reserved slot's cost (present only when `status == Reserved`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot_cost: Option<i64>,
}

/// Resolve the set of mandatory access capabilities for an applicant given the
/// catalog's `hardRequirements` mapping. Returned as a sorted set so the
/// result never depends on the order needs were supplied.
fn required_access(catalog: &Catalog, req: &RouteRequest) -> BTreeSet<String> {
    let mut needs: BTreeSet<String> = BTreeSet::new();
    if let Some(m) = &req.mobility {
        if let Some(cap) = catalog.hard_requirements.get(m) {
            needs.insert(cap.clone());
        }
    }
    for c in &req.communication {
        if let Some(cap) = catalog.hard_requirements.get(c) {
            needs.insert(cap.clone());
        }
    }
    needs
}

/// Pure routing function over an immutable catalog snapshot.
pub fn route(catalog: &Catalog, req: &RouteRequest) -> RouteResult {
    let evaluated_at = req.at.unwrap_or_else(Utc::now);
    let origin = crate::model::Grid { x: req.origin[0], y: req.origin[1] };
    let needed_access = required_access(catalog, req);

    // Normalized communication list for the persisted, order-independent record.
    let communication: Vec<String> =
        req.communication.iter().cloned().collect::<BTreeSet<_>>().into_iter().collect();

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut exclusions: Vec<Exclusion> = Vec::new();

    // Points iterate in id order (BTreeMap) for deterministic exclusion order.
    for (id, point) in &catalog.points {
        let mut reasons: Vec<String> = Vec::new();

        // Hard check 1: service category.
        if !point.services.contains(&req.service) {
            reasons.push("MISSING_SERVICE".to_string());
        }

        // Hard check 2: every mandatory access capability that is *structurally*
        // absent from the point. Reasons are emitted in sorted capability order.
        for cap in &needed_access {
            if !point.access.contains(cap) {
                reasons.push(format!("MISSING_ACCESS:{cap}"));
            }
        }

        // Temporal checks. A CLOSURE dominates a capability DEGRADATION: while a
        // point is closed the sole temporal reason is `CLOSED:<eventId>` and any
        // overlapping degradation is suppressed (the whole point is unavailable,
        // so its individual capabilities are moot). Only when the point is open
        // do degradations remove otherwise-present capabilities.
        let mut closed = false;
        if let Some(list) = catalog.closures_by_point.get(id) {
            for c in list {
                if c.covers(evaluated_at) {
                    reasons.push(format!("CLOSED:{}", c.event_id));
                    closed = true;
                }
            }
        }

        // Hard check 3: capability degradations, only when the point is open.
        // For each needed capability that the point *has* structurally but that
        // is degraded at the instant, report `DEGRADED:<eventId>:<cap>`. If
        // several degradations remove the same capability the lowest event id
        // wins (deterministic), since the list is sorted by event id.
        if !closed {
            if let Some(list) = catalog.degradations_by_point.get(id) {
                for cap in &needed_access {
                    if !point.access.contains(cap) {
                        continue; // already reported as MISSING_ACCESS
                    }
                    if let Some(active) =
                        list.iter().find(|d| &d.capability == cap && d.covers(evaluated_at))
                    {
                        reasons.push(format!("DEGRADED:{}:{}", active.event_id, cap));
                    }
                }
            }
        }

        if reasons.is_empty() {
            let manhattan = origin.manhattan(&point.grid);
            let total = manhattan + point.barrier_penalty;
            candidates.push(Candidate {
                point_id: id.clone(),
                grid: [point.grid.x, point.grid.y],
                cost: CostBreakdown {
                    manhattan,
                    barrier_penalty: point.barrier_penalty,
                    total,
                },
            });
        } else {
            exclusions.push(Exclusion { point_id: id.clone(), reasons });
        }
    }

    // Tie-break: total cost ascending, then point id ascending. Because point
    // ids are unique this is a total order -> fully stable regardless of the
    // input order the points arrived in.
    candidates.sort_by(|a, b| {
        a.cost
            .total
            .cmp(&b.cost.total)
            .then_with(|| a.point_id.cmp(&b.point_id))
    });

    // Home-service fallback (round 3). It is a *degraded path*, entered ONLY
    // when all three hold:
    //   1. the applicant's mobility is the catalog's allowed home mobility
    //      (`HOMEBOUND`),
    //   2. the requested service is the allowed home service (`LEGAL_AID`),
    //   3. every physical point was excluded (no surviving candidate).
    // The physical exclusion chain is preserved untouched. Capacity is *not*
    // resolved here (that is stateful and belongs to the store); the pure
    // function only marks eligibility with status `Eligible`.
    let home_service = catalog.home_service.as_ref().and_then(|hs| {
        let mobility_ok = req
            .mobility
            .as_ref()
            .map(|m| hs.allowed_mobility.contains(m))
            .unwrap_or(false);
        let eligible = mobility_ok && req.service == hs.allowed_service && candidates.is_empty();
        if eligible {
            Some(HomeServiceOutcome {
                reason: hs.reason.clone(),
                status: HomeServiceStatus::Eligible,
                slot_id: None,
                slot_cost: None,
            })
        } else {
            None
        }
    });

    RouteResult {
        catalog_version: catalog.version.clone(),
        origin: req.origin,
        service: req.service.clone(),
        mobility: req.mobility.clone(),
        communication,
        evaluated_at,
        candidates,
        exclusions,
        home_service,
    }
}
