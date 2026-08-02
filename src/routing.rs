use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::catalog::{parse_rfc3339, Catalog};
use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RouteRequest {
    #[serde(rename = "originGrid")]
    pub origin_grid: [i32; 2],
    pub service: String,
    #[serde(default)]
    pub mobility: Vec<String>,
    #[serde(default)]
    pub communication: Vec<String>,
    #[serde(rename = "queryTime")]
    pub query_time: Option<String>,
}

impl RouteRequest {
    pub fn needs(&self) -> Vec<String> {
        let mut all = Vec::new();
        all.extend(self.mobility.iter().cloned());
        all.extend(self.communication.iter().cloned());
        all
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CostBreakdown {
    pub distance: i32,
    #[serde(rename = "barrierPenalty")]
    pub barrier_penalty: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Candidate {
    pub rank: usize,
    #[serde(rename = "pointId")]
    pub point_id: String,
    #[serde(rename = "totalCost")]
    pub total_cost: i32,
    #[serde(rename = "costBreakdown")]
    pub cost: CostBreakdown,
    pub grid: [i32; 2],
    pub services: Vec<String>,
    pub access: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Excluded {
    #[serde(rename = "pointId")]
    pub point_id: String,
    pub reasons: Vec<ExclusionReason>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExclusionReason {
    MissingService {
        service: String,
    },
    MissingAccess {
        required: String,
    },
    TemporarilyClosed {
        #[serde(rename = "eventId")]
        event_id: String,
        from: String,
        to: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct HomeServiceResult {
    pub eligible: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RouteResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home_service: Option<HomeServiceResult>,
    pub candidates: Vec<Candidate>,
    pub excluded: Vec<Excluded>,
    #[serde(rename = "tieBreak")]
    pub tie_break: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RouteResponse {
    #[serde(rename = "snapshotId")]
    pub snapshot_id: String,
    #[serde(rename = "catalogVersion")]
    pub catalog_version: String,
    #[serde(rename = "queryTime")]
    pub query_time: String,
    #[serde(rename = "originGrid")]
    pub origin_grid: [i32; 2],
    pub service: String,
    #[serde(rename = "homeService", skip_serializing_if = "Option::is_none")]
    pub home_service: Option<HomeServiceResult>,
    pub candidates: Vec<Candidate>,
    pub excluded: Vec<Excluded>,
    #[serde(rename = "tieBreak")]
    pub tie_break: Vec<String>,
}

pub fn resolve_query_time(req: &RouteRequest) -> AppResult<DateTime<Utc>> {
    match &req.query_time {
        Some(s) => parse_rfc3339(s),
        None => Ok(Utc::now()),
    }
}

pub fn compute(catalog: &Catalog, req: &RouteRequest, query_time: DateTime<Utc>) -> RouteResult {
    let required_access = resolve_required_access(catalog, req);

    let home_service = resolve_home_service(catalog, req);

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut excluded: Vec<Excluded> = Vec::new();

    for point in &catalog.points {
        let mut reasons: Vec<ExclusionReason> = Vec::new();

        let has_service = point.services.iter().any(|s| s == &req.service);
        if !has_service {
            reasons.push(ExclusionReason::MissingService {
                service: req.service.clone(),
            });
        }

        for req_access in &required_access {
            if !point.access.iter().any(|a| a == req_access) {
                reasons.push(ExclusionReason::MissingAccess {
                    required: req_access.clone(),
                });
            }
        }

        let services_set: BTreeSet<&str> = point.services.iter().map(|s| s.as_str()).collect();
        let access_set: BTreeSet<&str> = point.access.iter().map(|a| a.as_str()).collect();

        for closure in &catalog.closures {
            if closure.point_id != point.id {
                continue;
            }
            let closed = query_time >= closure.from && query_time < closure.to;
            if closed {
                reasons.push(ExclusionReason::TemporarilyClosed {
                    event_id: closure.event_id.clone(),
                    from: closure.from.to_rfc3339(),
                    to: closure.to.to_rfc3339(),
                });
            }
        }

        if reasons.is_empty() {
            let distance =
                (req.origin_grid[0] - point.grid[0]).abs() + (req.origin_grid[1] - point.grid[1]).abs();
            let total_cost = distance + point.barrier_penalty;
            candidates.push(Candidate {
                rank: 0,
                point_id: point.id.clone(),
                total_cost,
                cost: CostBreakdown {
                    distance,
                    barrier_penalty: point.barrier_penalty,
                },
                grid: point.grid,
                services: services_set.into_iter().map(|s| s.to_string()).collect(),
                access: access_set.into_iter().map(|s| s.to_string()).collect(),
            });
        } else {
            excluded.push(Excluded {
                point_id: point.id.clone(),
                reasons,
            });
        }
    }

    candidates.sort_by(|a, b| {
        a.total_cost
            .cmp(&b.total_cost)
            .then_with(|| a.point_id.cmp(&b.point_id))
    });
    for (i, c) in candidates.iter_mut().enumerate() {
        c.rank = i + 1;
    }

    excluded.sort_by(|a, b| a.point_id.cmp(&b.point_id));
    for e in &mut excluded {
        e.reasons.sort_by(compare_reasons);
    }

    RouteResult {
        home_service,
        candidates,
        excluded,
        tie_break: catalog.tie_break.clone(),
    }
}

fn compare_reasons(a: &ExclusionReason, b: &ExclusionReason) -> std::cmp::Ordering {
    use ExclusionReason::*;
    let rank = |r: &ExclusionReason| -> u8 {
        match r {
            MissingService { .. } => 0,
            MissingAccess { .. } => 1,
            TemporarilyClosed { .. } => 2,
        }
    };
    rank(a)
        .cmp(&rank(b))
        .then_with(|| {
            let sa = serde_json::to_string(a).unwrap_or_default();
            let sb = serde_json::to_string(b).unwrap_or_default();
            sa.cmp(&sb)
        })
}

fn resolve_required_access(catalog: &Catalog, req: &RouteRequest) -> Vec<String> {
    let mut set = BTreeSet::new();
    for need in req.needs() {
        if let Some(access) = catalog.hard_requirements.get(&need) {
            set.insert(access.clone());
        }
    }
    set.into_iter().collect()
}

fn resolve_home_service(catalog: &Catalog, req: &RouteRequest) -> Option<HomeServiceResult> {
    if req.service != catalog.home_service.allowed_service {
        return None;
    }
    let allowed: BTreeSet<&str> = catalog
        .home_service
        .allowed_mobility
        .iter()
        .map(|s| s.as_str())
        .collect();
    let eligible = req.mobility.iter().any(|m| allowed.contains(m.as_str()));
    if eligible {
        Some(HomeServiceResult {
            eligible: true,
            reason: catalog.home_service.reason.clone(),
        })
    } else {
        None
    }
}

pub fn validate_request(req: &RouteRequest) -> AppResult<()> {
    if req.service.trim().is_empty() {
        return Err(AppError::BadRequest(
            "service must not be empty".to_string(),
        ));
    }
    if let Some(qt) = &req.query_time {
        parse_rfc3339(qt)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Closure, HomeServiceConfig, Point};
    use chrono::{Duration, TimeZone, Utc};
    use std::collections::HashMap;

    fn closure_window() -> (DateTime<Utc>, DateTime<Utc>) {
        let from = Utc.with_ymd_and_hms(2026, 8, 2, 0, 0, 0).unwrap();
        let to = Utc.with_ymd_and_hms(2026, 8, 4, 0, 0, 0).unwrap();
        (from, to)
    }

    fn open_time() -> DateTime<Utc> {
        let (_, to) = closure_window();
        to + Duration::days(1)
    }

    fn sample_catalog() -> Catalog {
        let (from, to) = closure_window();
        let mut hard = HashMap::new();
        hard.insert("WHEELCHAIR".into(), "STEP_FREE".into());
        hard.insert("HEARING".into(), "SIGN_INTERPRETER".into());
        hard.insert("SPEECH".into(), "TEXT_COMMUNICATION".into());

        Catalog {
            catalog_version: "CAT-TEST".into(),
            cost_formula: "abs(originGridX-pointGridX)+abs(originGridY-pointGridY)+barrierPenalty".into(),
            points: vec![
                Point {
                    id: "POINT-A".into(),
                    grid: [1, 1],
                    services: vec!["LEGAL_AID".into(), "MEDIATION".into()],
                    access: vec!["STEP_FREE".into(), "TEXT_COMMUNICATION".into()],
                    barrier_penalty: 0,
                },
                Point {
                    id: "POINT-B".into(),
                    grid: [2, 1],
                    services: vec!["LEGAL_AID".into(), "NOTARY".into()],
                    access: vec!["SIGN_INTERPRETER".into()],
                    barrier_penalty: 1,
                },
                Point {
                    id: "POINT-C".into(),
                    grid: [5, 5],
                    services: vec!["LEGAL_AID".into(), "NOTARY".into(), "MEDIATION".into()],
                    access: vec![
                        "STEP_FREE".into(),
                        "SIGN_INTERPRETER".into(),
                        "TEXT_COMMUNICATION".into(),
                    ],
                    barrier_penalty: 0,
                },
            ],
            hard_requirements: hard,
            tie_break: vec!["totalCost ascending".into(), "point id ascending".into()],
            closures: vec![Closure {
                event_id: "CLOSE-01".into(),
                point_id: "POINT-C".into(),
                from,
                to,
            }],
            home_service: HomeServiceConfig {
                allowed_service: "LEGAL_AID".into(),
                allowed_mobility: vec!["HOMEBOUND".into()],
                reason: "HOME_SERVICE_REQUIRED".into(),
            },
        }
    }

    fn req(service: &str, mobility: &[&str], communication: &[&str], t: DateTime<Utc>) -> RouteRequest {
        RouteRequest {
            origin_grid: [2, 2],
            service: service.into(),
            mobility: mobility.iter().map(|s| s.to_string()).collect(),
            communication: communication.iter().map(|s| s.to_string()).collect(),
            query_time: Some(t.to_rfc3339()),
        }
    }

    #[test]
    fn test_basic_routing_and_tie_break_by_point_id() {
        let cat = sample_catalog();
        let t = open_time();
        let r = compute(&cat, &req("LEGAL_AID", &[], &[], t), t);
        let ids: Vec<_> = r.candidates.iter().map(|c| c.point_id.clone()).collect();
        assert_eq!(ids, vec!["POINT-A", "POINT-B", "POINT-C"]);
        assert_eq!(r.candidates[0].total_cost, 2);
        assert_eq!(r.candidates[1].total_cost, 2);
        assert_eq!(r.candidates[2].total_cost, 6);
        assert_eq!(r.candidates[0].rank, 1);
        assert_eq!(r.candidates[1].rank, 2);
    }

    #[test]
    fn test_wheelchair_filters_before_distance() {
        let cat = sample_catalog();
        let t = open_time();
        let r = compute(&cat, &req("LEGAL_AID", &["WHEELCHAIR"], &[], t), t);
        let ids: Vec<_> = r.candidates.iter().map(|c| c.point_id.clone()).collect();
        assert_eq!(ids, vec!["POINT-A", "POINT-C"]);
        let b = r.excluded.iter().find(|e| e.point_id == "POINT-B").unwrap();
        assert!(matches!(
            &b.reasons[0],
            ExclusionReason::MissingAccess { required } if required == "STEP_FREE"
        ));
        assert_eq!(r.candidates[1].total_cost, 6);
    }

    #[test]
    fn test_hearing_requirement() {
        let cat = sample_catalog();
        let t = open_time();
        let r = compute(&cat, &req("LEGAL_AID", &[], &["HEARING"], t), t);
        let ids: Vec<_> = r.candidates.iter().map(|c| c.point_id.clone()).collect();
        assert_eq!(ids, vec!["POINT-B", "POINT-C"]);
    }

    #[test]
    fn test_speech_requirement() {
        let cat = sample_catalog();
        let t = open_time();
        let r = compute(&cat, &req("LEGAL_AID", &[], &["SPEECH"], t), t);
        let ids: Vec<_> = r.candidates.iter().map(|c| c.point_id.clone()).collect();
        assert_eq!(ids, vec!["POINT-A", "POINT-C"]);
    }

    #[test]
    fn test_all_three_requirements_only_c() {
        let cat = sample_catalog();
        let t = open_time();
        let r = compute(
            &cat,
            &req("LEGAL_AID", &["WHEELCHAIR"], &["HEARING", "SPEECH"], t),
            t,
        );
        let ids: Vec<_> = r.candidates.iter().map(|c| c.point_id.clone()).collect();
        assert_eq!(ids, vec!["POINT-C"]);
    }

    #[test]
    fn test_no_candidates_when_all_hard_capabilities_unsatisfied() {
        let cat = sample_catalog();
        let (from, _) = closure_window();
        let r = compute(
            &cat,
            &req("NOTARY", &["WHEELCHAIR"], &["HEARING"], from),
            from,
        );
        assert!(r.candidates.is_empty());
        let excluded_ids: Vec<_> = r.excluded.iter().map(|e| e.point_id.clone()).collect();
        assert!(excluded_ids.contains(&"POINT-A".to_string()));
        assert!(excluded_ids.contains(&"POINT-B".to_string()));
        assert!(excluded_ids.contains(&"POINT-C".to_string()));
        let c = r.excluded.iter().find(|e| e.point_id == "POINT-C").unwrap();
        assert!(c.reasons.iter().any(|reason| matches!(
            reason,
            ExclusionReason::TemporarilyClosed { .. }
        )));
    }

    #[test]
    fn test_unknown_service_excludes_all() {
        let cat = sample_catalog();
        let (from, _) = closure_window();
        let r = compute(&cat, &req("IMMIGRATION", &[], &[], from), from);
        assert!(r.candidates.is_empty());
        assert_eq!(r.excluded.len(), 3);
        for e in &r.excluded {
            assert!(matches!(&e.reasons[0], ExclusionReason::MissingService { .. }));
        }
    }

    #[test]
    fn test_closure_start_boundary_is_closed() {
        let cat = sample_catalog();
        let (from, _) = closure_window();
        let r = compute(&cat, &req("LEGAL_AID", &[], &[], from), from);
        let c = r.excluded.iter().find(|e| e.point_id == "POINT-C").unwrap();
        assert!(matches!(&c.reasons[0], ExclusionReason::TemporarilyClosed { event_id, .. } if event_id == "CLOSE-01"));
    }

    #[test]
    fn test_closure_end_boundary_is_open() {
        let cat = sample_catalog();
        let (_, to) = closure_window();
        let r = compute(&cat, &req("LEGAL_AID", &[], &[], to), to);
        let ids: Vec<_> = r.candidates.iter().map(|c| c.point_id.clone()).collect();
        assert!(ids.contains(&"POINT-C".to_string()));
        assert!(r.excluded.iter().all(|e| e.point_id != "POINT-C"));
    }

    #[test]
    fn test_just_before_closure_is_open() {
        let cat = sample_catalog();
        let (from, _) = closure_window();
        let t = from - Duration::seconds(1);
        let r = compute(&cat, &req("LEGAL_AID", &[], &[], t), t);
        let ids: Vec<_> = r.candidates.iter().map(|c| c.point_id.clone()).collect();
        assert!(ids.contains(&"POINT-C".to_string()));
    }

    #[test]
    fn test_just_before_end_is_still_closed() {
        let cat = sample_catalog();
        let (_, to) = closure_window();
        let t = to - Duration::seconds(1);
        let r = compute(&cat, &req("LEGAL_AID", &[], &[], t), t);
        let c = r.excluded.iter().find(|e| e.point_id == "POINT-C").unwrap();
        assert!(matches!(&c.reasons[0], ExclusionReason::TemporarilyClosed { .. }));
    }

    #[test]
    fn test_home_service_eligible() {
        let cat = sample_catalog();
        let (from, _) = closure_window();
        let r = compute(&cat, &req("LEGAL_AID", &["HOMEBOUND"], &[], from), from);
        let hs = r.home_service.as_ref().expect("home service expected");
        assert!(hs.eligible);
        assert_eq!(hs.reason, "HOME_SERVICE_REQUIRED");
    }

    #[test]
    fn test_home_service_not_eligible_for_other_service() {
        let cat = sample_catalog();
        let (from, _) = closure_window();
        let r = compute(&cat, &req("NOTARY", &["HOMEBOUND"], &[], from), from);
        assert!(r.home_service.is_none());
    }

    #[test]
    fn test_home_service_not_eligible_without_homebound() {
        let cat = sample_catalog();
        let (from, _) = closure_window();
        let r = compute(&cat, &req("LEGAL_AID", &[], &[], from), from);
        assert!(r.home_service.is_none());
    }

    #[test]
    fn test_cost_breakdown_and_barrier_penalty() {
        let cat = sample_catalog();
        let (from, _) = closure_window();
        let r = compute(&cat, &req("LEGAL_AID", &[], &[], from), from);
        let b = r.candidates.iter().find(|c| c.point_id == "POINT-B").unwrap();
        assert_eq!(b.cost.distance, 1);
        assert_eq!(b.cost.barrier_penalty, 1);
        assert_eq!(b.total_cost, 2);
    }

    #[test]
    fn test_excluded_sorted_by_point_id_regardless_of_input_order() {
        let cat = sample_catalog();
        let (from, _) = closure_window();
        let r = compute(&cat, &req("IMMIGRATION", &[], &[], from), from);
        let ids: Vec<_> = r.excluded.iter().map(|e| e.point_id.clone()).collect();
        assert_eq!(ids, vec!["POINT-A", "POINT-B", "POINT-C"]);
    }

    #[test]
    fn test_multiple_exclusion_reasons_are_complete_and_ordered() {
        let mut cat = sample_catalog();
        cat.closures = vec![];
        cat.points.push(Point {
            id: "POINT-D".into(),
            grid: [9, 9],
            services: vec!["MEDIATION".into()],
            access: vec![],
            barrier_penalty: 3,
        });
        let (from, _) = closure_window();
        cat.closures.push(Closure {
            event_id: "CLOSE-D".into(),
            point_id: "POINT-D".into(),
            from,
            to: from + Duration::hours(48),
        });

        let r = compute(&cat, &req("LEGAL_AID", &["WHEELCHAIR"], &[], from), from);
        let d = r.excluded.iter().find(|e| e.point_id == "POINT-D").unwrap();
        assert!(matches!(&d.reasons[0], ExclusionReason::MissingService { .. }));
        assert!(matches!(&d.reasons[1], ExclusionReason::MissingAccess { required } if required == "STEP_FREE"));
        assert!(matches!(&d.reasons[2], ExclusionReason::TemporarilyClosed { .. }));
        assert_eq!(d.reasons.len(), 3);
    }

    #[test]
    fn test_input_order_does_not_affect_result() {
        let cat1 = sample_catalog();
        let mut cat2 = sample_catalog();
        cat2.points.reverse();
        cat2.closures.reverse();
        let t = open_time();
        let request = req("LEGAL_AID", &["WHEELCHAIR"], &["HEARING"], t);

        let r1 = compute(&cat1, &request, t);
        let r2 = compute(&cat2, &request, t);

        let ids1: Vec<_> = r1.candidates.iter().map(|c| (&c.point_id, c.total_cost, c.rank)).collect();
        let ids2: Vec<_> = r2.candidates.iter().map(|c| (&c.point_id, c.total_cost, c.rank)).collect();
        assert_eq!(ids1, ids2);

        let ex1: Vec<_> = r1.excluded.iter().map(|e| (&e.point_id, &e.reasons)).collect();
        let ex2: Vec<_> = r2.excluded.iter().map(|e| (&e.point_id, &e.reasons)).collect();
        assert_eq!(ex1, ex2);
    }

    #[test]
    fn test_duplicate_needs_collapse_to_single_access_requirement() {
        let cat = sample_catalog();
        let (from, _) = closure_window();
        let request = RouteRequest {
            origin_grid: [2, 2],
            service: "LEGAL_AID".into(),
            mobility: vec!["WHEELCHAIR".into()],
            communication: vec!["WHEELCHAIR".into()],
            query_time: Some(from.to_rfc3339()),
        };
        let r = compute(&cat, &request, from);
        let b = r.excluded.iter().find(|e| e.point_id == "POINT-B").unwrap();
        assert_eq!(b.reasons.len(), 1);
    }
}

