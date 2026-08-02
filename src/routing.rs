//! Pure routing logic. Hard capability gates are evaluated BEFORE cost is
//! compared: a nearer point that lacks a mandatory capability is excluded,
//! never preferred.

use crate::model::*;

pub const REASON_SERVICE_UNAVAILABLE: &str = "SERVICE_UNAVAILABLE";
pub const REASON_MISSING_ACCESS: &str = "MISSING_REQUIRED_ACCESS";
pub const REASON_CLOSED: &str = "TEMPORARILY_CLOSED";
pub const REASON_DEGRADED: &str = "CAPABILITY_DEGRADED";
pub const REASON_HOME_REQUIRED: &str = "HOME_SERVICE_REQUIRED";
pub const REASON_HOME_UNAVAILABLE: &str = "HOME_SERVICE_NOT_AVAILABLE";
pub const REASON_HOME_NO_CAPACITY: &str = "HOME_SERVICE_NO_CAPACITY";

/// Closures and capability degradations are active on the half-open interval
/// [from, to): the "from" instant is affected, the "to" instant is normal.
pub fn closure_active(c: &Closure, at_epoch: i64) -> bool {
    c.from_ts <= at_epoch && at_epoch < c.to_ts
}

pub fn degradation_active(e: &CapabilityEvent, at_epoch: i64) -> bool {
    e.from_ts <= at_epoch && at_epoch < e.to_ts
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
        // Event overlap priority: an active closure dominates capability
        // degradations at the same point — the whole location is closed, so
        // degradation reasons would be redundant noise in the chain.
        let active_closures: Vec<&Closure> = cat
            .closures
            .iter()
            .filter(|c| c.point_id == p.id && closure_active(c, req.at_epoch))
            .collect();
        if !active_closures.is_empty() {
            for c in active_closures {
                reasons.push(ExclusionReason {
                    code: REASON_CLOSED.to_string(),
                    capability: None,
                    event_id: Some(c.event_id.clone()),
                    from: Some(c.from_rfc3339.clone()),
                    to: Some(c.to_rfc3339.clone()),
                });
            }
        } else {
            // A degradation only excludes the point when the applicant
            // actually requires the degraded capability.
            for e in cat
                .capability_events
                .iter()
                .filter(|e| e.point_id == p.id && degradation_active(e, req.at_epoch))
            {
                if required.iter().any(|r| r == &e.capability) {
                    reasons.push(ExclusionReason {
                        code: REASON_DEGRADED.to_string(),
                        capability: Some(e.capability.clone()),
                        event_id: Some(e.event_id.clone()),
                        from: Some(e.from_rfc3339.clone()),
                        to: Some(e.to_rfc3339.clone()),
                    });
                }
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

    let home_service = home_service_outcome(cat, req, &candidates, &exclusions);

    RouteOutcome {
        snapshot_id,
        catalog_version: cat.version.clone(),
        cost_formula: cat.cost_formula.clone(),
        request: req.clone(),
        candidates,
        exclusions,
        home_service,
    }
}

/// Hard-condition reason codes: failures of mandatory capabilities or of
/// availability events. The home-service degradation path triggers only when
/// every entity point failed exclusively on such conditions — a point that
/// merely does not offer the service is not an accessibility failure.
fn is_hard_condition(code: &str) -> bool {
    matches!(code, REASON_MISSING_ACCESS | REASON_CLOSED | REASON_DEGRADED)
}

/// Home service is a degradation path, never a shortcut: it is offered only
/// when the applicant's mobility is registered for it AND no entity point
/// survived the hard gates. The entity exclusion chains are left untouched
/// so the caller can see exactly why each physical point failed.
fn home_service_outcome(
    cat: &Catalog,
    req: &NormalizedRequest,
    candidates: &[Candidate],
    exclusions: &[Exclusion],
) -> Option<HomeServiceOutcome> {
    let hs = cat.home_service.as_ref()?;
    if !hs.allowed_mobility.iter().any(|m| m == &req.mobility) {
        return None;
    }
    if !candidates.is_empty() {
        return None;
    }
    let hard_only = !exclusions.is_empty()
        && exclusions.iter().all(|e| {
            !e.reasons.is_empty() && e.reasons.iter().all(|r| is_hard_condition(&r.code))
        });
    if hs.allowed_service == req.service_need && hard_only {
        Some(HomeServiceOutcome {
            eligible: true,
            reason: hs.reason.clone(),
            booking: None,
        })
    } else {
        Some(HomeServiceOutcome {
            eligible: false,
            reason: REASON_HOME_UNAVAILABLE.to_string(),
            booking: None,
        })
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
    fn degradation_window_is_half_open() {
        let e = CapabilityEvent {
            event_id: "D".into(),
            point_id: "P".into(),
            capability: "SIGN_INTERPRETER".into(),
            from_ts: 100,
            to_ts: 200,
            from_rfc3339: String::new(),
            to_rfc3339: String::new(),
        };
        assert!(!degradation_active(&e, 99));
        assert!(degradation_active(&e, 100), "from endpoint is degraded");
        assert!(degradation_active(&e, 199));
        assert!(!degradation_active(&e, 200), "to endpoint is normal again");
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
