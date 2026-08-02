//! Pure routing logic. Hard capability gates are evaluated BEFORE cost is
//! compared: a nearer point that lacks a mandatory capability is excluded,
//! never preferred.

use crate::model::*;

pub const REASON_SERVICE_UNAVAILABLE: &str = "SERVICE_UNAVAILABLE";
pub const REASON_MISSING_ACCESS: &str = "MISSING_REQUIRED_ACCESS";
pub const REASON_CLOSED: &str = "TEMPORARILY_CLOSED";
pub const REASON_HOME_REQUIRED: &str = "HOME_SERVICE_REQUIRED";
pub const REASON_HOME_UNAVAILABLE: &str = "HOME_SERVICE_NOT_AVAILABLE";

/// A closure is active on the half-open interval [from, to): the "from"
/// instant is closed, the "to" instant is open again.
pub fn closure_active(c: &Closure, at_epoch: i64) -> bool {
    c.from_ts <= at_epoch && at_epoch < c.to_ts
}

/// Native implementation of the catalog cost formula
/// `abs(originGridX-pointGridX)+abs(originGridY-pointGridY)+barrierPenalty`.
pub fn total_cost(origin: &GridOrigin, p: &Point) -> CostBreakdown {
    let distance = (origin.x - p.grid_x).abs() + (origin.y - p.grid_y).abs();
    CostBreakdown {
        distance,
        barrier_penalty: p.barrier_penalty,
    }
}

/// Resolve the required access capabilities for a request from the catalog's
/// hardRequirements map. Unknown needs (mobility or communication entries
/// with no mapping and not STANDARD) are rejected by the API layer.
pub fn required_access(cat: &Catalog, req: &NormalizedRequest) -> Vec<String> {
    let mut reqd: Vec<String> = Vec::new();
    if let Some(a) = cat.hard_requirements.get(&req.mobility) {
        reqd.push(a.clone());
    }
    for c in &req.communication {
        if let Some(a) = cat.hard_requirements.get(c) {
            reqd.push(a.clone());
        }
    }
    reqd.sort();
    reqd.dedup();
    reqd
}

pub fn route(cat: &Catalog, req: &NormalizedRequest, snapshot_id: String) -> RouteOutcome {
    let required = required_access(cat, req);

    // Homebound applicants cannot travel: every physical point is excluded.
    if let Some(hs) = &cat.home_service {
        if hs.allowed_mobility.iter().any(|m| m == &req.mobility) {
            let eligible = hs.allowed_service == req.service_need;
            let reason = if eligible {
                hs.reason.clone()
            } else {
                REASON_HOME_UNAVAILABLE.to_string()
            };
            let code = if eligible {
                REASON_HOME_REQUIRED
            } else {
                REASON_HOME_UNAVAILABLE
            };
            let exclusions = cat
                .points
                .iter()
                .map(|p| Exclusion {
                    point_id: p.id.clone(),
                    reasons: vec![ExclusionReason::simple(code)],
                })
                .collect();
            return RouteOutcome {
                snapshot_id,
                catalog_version: cat.version.clone(),
                cost_formula: cat.cost_formula.clone(),
                request: req.clone(),
                candidates: Vec::new(),
                exclusions,
                home_service: Some(HomeServiceOutcome { eligible, reason }),
            };
        }
    }

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut exclusions: Vec<Exclusion> = Vec::new();

    for p in &cat.points {
        let mut reasons: Vec<ExclusionReason> = Vec::new();

        if !p.services.iter().any(|s| s == &req.service_need) {
            reasons.push(ExclusionReason::simple(REASON_SERVICE_UNAVAILABLE));
        }
        for cap in &required {
            if !p.access.iter().any(|a| a == cap) {
                reasons.push(ExclusionReason {
                    code: REASON_MISSING_ACCESS.to_string(),
                    capability: Some(cap.clone()),
                    event_id: None,
                    from: None,
                    to: None,
                });
            }
        }
        for c in cat.closures.iter().filter(|c| c.point_id == p.id) {
            if closure_active(c, req.at_epoch) {
                reasons.push(ExclusionReason {
                    code: REASON_CLOSED.to_string(),
                    capability: None,
                    event_id: Some(c.event_id.clone()),
                    from: Some(c.from_rfc3339.clone()),
                    to: Some(c.to_rfc3339.clone()),
                });
            }
        }

        if reasons.is_empty() {
            let breakdown = total_cost(&req.origin, p);
            candidates.push(Candidate {
                point_id: p.id.clone(),
                total_cost: breakdown.distance + breakdown.barrier_penalty,
                cost_breakdown: breakdown,
                services: p.services.clone(),
                access: p.access.clone(),
            });
        } else {
            exclusions.push(Exclusion {
                point_id: p.id.clone(),
                reasons,
            });
        }
    }

    // Deterministic ordering per the catalog tieBreak rule:
    // totalCost ascending, then point id ascending. Exclusions by point id.
    candidates.sort_by(|a, b| {
        a.total_cost
            .cmp(&b.total_cost)
            .then_with(|| a.point_id.cmp(&b.point_id))
    });
    exclusions.sort_by(|a, b| a.point_id.cmp(&b.point_id));

    RouteOutcome {
        snapshot_id,
        catalog_version: cat.version.clone(),
        cost_formula: cat.cost_formula.clone(),
        request: req.clone(),
        candidates,
        exclusions,
        home_service: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closure(from: i64, to: i64) -> Closure {
        Closure {
            event_id: "E".into(),
            point_id: "P".into(),
            from_ts: from,
            to_ts: to,
            from_rfc3339: String::new(),
            to_rfc3339: String::new(),
        }
    }

    #[test]
    fn closure_window_is_half_open() {
        let c = closure(100, 200);
        assert!(!closure_active(&c, 99));
        assert!(closure_active(&c, 100), "from endpoint is closed");
        assert!(closure_active(&c, 199));
        assert!(!closure_active(&c, 200), "to endpoint is open again");
    }

    #[test]
    fn cost_is_manhattan_plus_barrier_penalty() {
        let p = Point {
            id: "P".into(),
            grid_x: 4,
            grid_y: -1,
            barrier_penalty: 3,
            services: vec![],
            access: vec![],
        };
        let b = total_cost(&GridOrigin { x: 1, y: 2 }, &p);
        assert_eq!(b.distance, 6);
        assert_eq!(b.barrier_penalty, 3);
    }
}
