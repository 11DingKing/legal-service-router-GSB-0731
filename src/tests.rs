use crate::db::Db;
use crate::models::{
    Catalog, Closure, Communication, HardRequirements, HomeService, Mobility, RouteRequest,
    ServicePoint,
};
use crate::router;
use std::collections::BTreeSet;
use std::sync::Arc;
use uuid::Uuid;

fn sample_catalog() -> Catalog {
    Catalog {
        catalog_version: "CAT-TEST-1".to_string(),
        cost_formula: "abs(originGridX-pointGridX)+abs(originGridY-pointGridY)+barrierPenalty"
            .to_string(),
        points: vec![
            ServicePoint {
                id: "POINT-A".to_string(),
                grid: [1, 1],
                services: BTreeSet::from(["LEGAL_AID".to_string(), "MEDIATION".to_string()]),
                access: BTreeSet::from([
                    "STEP_FREE".to_string(),
                    "TEXT_COMMUNICATION".to_string(),
                ]),
                barrier_penalty: 0,
            },
            ServicePoint {
                id: "POINT-B".to_string(),
                grid: [2, 1],
                services: BTreeSet::from(["LEGAL_AID".to_string(), "NOTARY".to_string()]),
                access: BTreeSet::from(["SIGN_INTERPRETER".to_string()]),
                barrier_penalty: 1,
            },
            ServicePoint {
                id: "POINT-C".to_string(),
                grid: [5, 5],
                services: BTreeSet::from([
                    "LEGAL_AID".to_string(),
                    "NOTARY".to_string(),
                    "MEDIATION".to_string(),
                ]),
                access: BTreeSet::from([
                    "STEP_FREE".to_string(),
                    "SIGN_INTERPRETER".to_string(),
                    "TEXT_COMMUNICATION".to_string(),
                ]),
                barrier_penalty: 0,
            },
        ],
        hard_requirements: HardRequirements {
            wheelchair: "STEP_FREE".to_string(),
            hearing: "SIGN_INTERPRETER".to_string(),
            speech: "TEXT_COMMUNICATION".to_string(),
        },
        tie_break: vec!["totalCost ascending".to_string(), "point id ascending".to_string()],
        closures: vec![Closure {
            event_id: "CLOSE-01".to_string(),
            point_id: "POINT-C".to_string(),
            from: chrono::DateTime::parse_from_rfc3339("2026-08-02T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            to: chrono::DateTime::parse_from_rfc3339("2026-08-04T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        }],
        home_service: HomeService {
            allowed_service: "LEGAL_AID".to_string(),
            allowed_mobility: BTreeSet::from(["HOMEBOUND".to_string()]),
            reason: "HOME_SERVICE_REQUIRED".to_string(),
        },
    }
}

fn tied_cost_catalog() -> Catalog {
    let mut cat = sample_catalog();
    cat.catalog_version = "CAT-TIED".to_string();
    cat.points = vec![
        ServicePoint {
            id: "POINT-Z".to_string(),
            grid: [0, 0],
            services: BTreeSet::from(["LEGAL_AID".to_string()]),
            access: BTreeSet::new(),
            barrier_penalty: 0,
        },
        ServicePoint {
            id: "POINT-A".to_string(),
            grid: [0, 0],
            services: BTreeSet::from(["LEGAL_AID".to_string()]),
            access: BTreeSet::new(),
            barrier_penalty: 0,
        },
        ServicePoint {
            id: "POINT-M".to_string(),
            grid: [0, 0],
            services: BTreeSet::from(["LEGAL_AID".to_string()]),
            access: BTreeSet::new(),
            barrier_penalty: 0,
        },
    ];
    cat.closures = vec![];
    cat
}

#[test]
fn filters_missing_service_before_cost() {
    let catalog = sample_catalog();
    let req = RouteRequest {
        origin_grid: [1, 1],
        service: "NOTARY".to_string(),
        mobility: vec![],
        communication: vec![],
        request_time: Some(
            chrono::DateTime::parse_from_rfc3339("2026-08-05T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ),
    };
    let resp = router::route(&catalog, &req, "snap-1".to_string());
    let ids: Vec<&str> = resp.candidates.iter().map(|c| c.point_id.as_str()).collect();
    assert_eq!(ids, vec!["POINT-B", "POINT-C"]);
    let excluded: Vec<&str> = resp.exclusions.iter().map(|e| e.point_id.as_str()).collect();
    assert!(excluded.contains(&"POINT-A"));
    assert!(resp
        .exclusions
        .iter()
        .any(|e| e.reason == "MISSING_SERVICE"));
}

#[test]
fn wheelchair_requires_step_free_and_cannot_be_overridden_by_distance() {
    let catalog = sample_catalog();
    let req = RouteRequest {
        origin_grid: [2, 1],
        service: "LEGAL_AID".to_string(),
        mobility: vec![Mobility::Wheelchair],
        communication: vec![],
        request_time: None,
    };
    let resp = router::route(&catalog, &req, "snap-2".to_string());
    let ids: Vec<&str> = resp.candidates.iter().map(|c| c.point_id.as_str()).collect();
    assert!(!ids.contains(&"POINT-B"), "POINT-B lacks STEP_FREE");
    assert!(ids.contains(&"POINT-A"));
    assert!(resp
        .exclusions
        .iter()
        .any(|e| e.point_id == "POINT-B" && e.reason == "MISSING_ACCESSIBILITY"));
}

#[test]
fn hearing_requires_sign_interpreter() {
    let catalog = sample_catalog();
    let req = RouteRequest {
        origin_grid: [1, 1],
        service: "LEGAL_AID".to_string(),
        mobility: vec![],
        communication: vec![Communication::Hearing],
        request_time: Some(
            chrono::DateTime::parse_from_rfc3339("2026-08-05T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ),
    };
    let resp = router::route(&catalog, &req, "snap-3".to_string());
    let ids: Vec<&str> = resp.candidates.iter().map(|c| c.point_id.as_str()).collect();
    assert_eq!(ids, vec!["POINT-B", "POINT-C"]);
}

#[test]
fn all_hard_capabilities_miss_returns_no_candidates() {
    let catalog = sample_catalog();
    let req = RouteRequest {
        origin_grid: [1, 1],
        service: "MEDIATION".to_string(),
        mobility: vec![Mobility::Wheelchair],
        communication: vec![Communication::Hearing],
        request_time: Some(
            chrono::DateTime::parse_from_rfc3339("2026-08-03T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ),
    };
    let resp = router::route(&catalog, &req, "snap-4".to_string());
    assert!(resp.candidates.is_empty());
    let reasons: Vec<&str> = resp.exclusions.iter().map(|e| e.reason.as_str()).collect();
    assert!(reasons.contains(&"MISSING_ACCESSIBILITY"));
    assert!(reasons.contains(&"TEMPORARILY_CLOSED"));
}

#[test]
fn closure_excludes_point_during_window() {
    let catalog = sample_catalog();
    let req = RouteRequest {
        origin_grid: [5, 5],
        service: "LEGAL_AID".to_string(),
        mobility: vec![],
        communication: vec![],
        request_time: Some(
            chrono::DateTime::parse_from_rfc3339("2026-08-03T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ),
    };
    let resp = router::route(&catalog, &req, "snap-5".to_string());
    let ids: Vec<&str> = resp.candidates.iter().map(|c| c.point_id.as_str()).collect();
    assert!(!ids.contains(&"POINT-C"));
    assert!(resp
        .exclusions
        .iter()
        .any(|e| e.point_id == "POINT-C" && e.reason == "TEMPORARILY_CLOSED"));
}

#[test]
fn closure_start_endpoint_is_inclusive_end_is_exclusive() {
    let catalog = sample_catalog();
    let req_before_end = RouteRequest {
        origin_grid: [5, 5],
        service: "LEGAL_AID".to_string(),
        mobility: vec![],
        communication: vec![],
        request_time: Some(
            chrono::DateTime::parse_from_rfc3339("2026-08-03T23:59:59Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ),
    };
    let resp = router::route(&catalog, &req_before_end, "snap-6".to_string());
    assert!(resp
        .exclusions
        .iter()
        .any(|e| e.point_id == "POINT-C" && e.reason == "TEMPORARILY_CLOSED"));

    let req_at_start = RouteRequest {
        origin_grid: [5, 5],
        service: "LEGAL_AID".to_string(),
        mobility: vec![],
        communication: vec![],
        request_time: Some(
            chrono::DateTime::parse_from_rfc3339("2026-08-02T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ),
    };
    let resp = router::route(&catalog, &req_at_start, "snap-7".to_string());
    assert!(resp
        .exclusions
        .iter()
        .any(|e| e.point_id == "POINT-C" && e.reason == "TEMPORARILY_CLOSED"));

    let req_after_end = RouteRequest {
        origin_grid: [5, 5],
        service: "LEGAL_AID".to_string(),
        mobility: vec![],
        communication: vec![],
        request_time: Some(
            chrono::DateTime::parse_from_rfc3339("2026-08-04T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ),
    };
    let resp = router::route(&catalog, &req_after_end, "snap-8".to_string());
    assert!(resp.candidates.iter().any(|c| c.point_id == "POINT-C"));
}

#[test]
fn homebound_applicant_gets_home_service_reason() {
    let catalog = sample_catalog();
    let req = RouteRequest {
        origin_grid: [1, 1],
        service: "LEGAL_AID".to_string(),
        mobility: vec![Mobility::Homebound],
        communication: vec![],
        request_time: None,
    };
    let resp = router::route(&catalog, &req, "snap-9".to_string());
    assert!(resp.candidates.is_empty());
    assert!(resp
        .exclusions
        .iter()
        .all(|e| e.reason == "HOME_SERVICE_REQUIRED"));
}

#[test]
fn cost_breakdown_includes_distance_and_barrier() {
    let catalog = sample_catalog();
    let req = RouteRequest {
        origin_grid: [0, 0],
        service: "LEGAL_AID".to_string(),
        mobility: vec![],
        communication: vec![],
        request_time: None,
    };
    let resp = router::route(&catalog, &req, "snap-10".to_string());
    let a = resp.candidates.iter().find(|c| c.point_id == "POINT-A").unwrap();
    assert_eq!(a.distance, 2);
    assert_eq!(a.barrier_penalty, 0);
    assert_eq!(a.total_cost, 2);
    let b = resp.candidates.iter().find(|c| c.point_id == "POINT-B").unwrap();
    assert_eq!(b.distance, 3);
    assert_eq!(b.barrier_penalty, 1);
    assert_eq!(b.total_cost, 4);
}

#[test]
fn tie_break_is_stable_by_point_id() {
    let catalog = tied_cost_catalog();
    let req = RouteRequest {
        origin_grid: [0, 0],
        service: "LEGAL_AID".to_string(),
        mobility: vec![],
        communication: vec![],
        request_time: None,
    };
    let resp = router::route(&catalog, &req, "snap-11".to_string());
    let ids: Vec<&str> = resp.candidates.iter().map(|c| c.point_id.as_str()).collect();
    assert_eq!(ids, vec!["POINT-A", "POINT-M", "POINT-Z"]);
}

#[test]
fn ordering_is_deterministic_regardless_of_input_order() {
    let c1 = tied_cost_catalog();
    let mut c2 = tied_cost_catalog();
    c2.points.reverse();
    let req = RouteRequest {
        origin_grid: [0, 0],
        service: "LEGAL_AID".to_string(),
        mobility: vec![],
        communication: vec![],
        request_time: None,
    };
    let r1 = router::route(&c1, &req, "a".to_string());
    let r2 = router::route(&c2, &req, "b".to_string());
    let ids1: Vec<&str> = r1.candidates.iter().map(|c| c.point_id.as_str()).collect();
    let ids2: Vec<&str> = r2.candidates.iter().map(|c| c.point_id.as_str()).collect();
    assert_eq!(ids1, ids2);
    let ex1: Vec<(&str, &str)> = r1
        .exclusions
        .iter()
        .map(|e| (e.point_id.as_str(), e.reason.as_str()))
        .collect();
    let ex2: Vec<(&str, &str)> = r2
        .exclusions
        .iter()
        .map(|e| (e.point_id.as_str(), e.reason.as_str()))
        .collect();
    assert_eq!(ex1, ex2);
}

#[test]
fn snapshot_immutability_after_hot_reload() {
    let db = Db::open_in_memory().unwrap();
    db.import_catalog(&sample_catalog()).unwrap();
    let catalog_v1 = db.load_current_catalog().unwrap().unwrap();
    let req = RouteRequest {
        origin_grid: [5, 5],
        service: "LEGAL_AID".to_string(),
        mobility: vec![],
        communication: vec![],
        request_time: Some(
            chrono::DateTime::parse_from_rfc3339("2026-08-03T12:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        ),
    };
    let resp_v1 = router::route(&catalog_v1, &req, "snap-v1".to_string());
    db.save_snapshot(
        "snap-v1",
        &catalog_v1.catalog_version,
        &serde_json::to_string(&req).unwrap(),
        &serde_json::to_string(&resp_v1).unwrap(),
    )
    .unwrap();

    let mut catalog_v2 = sample_catalog();
    catalog_v2.catalog_version = "CAT-TEST-2".to_string();
    catalog_v2.closures = vec![];
    db.import_catalog(&catalog_v2).unwrap();

    let stored = db.load_snapshot("snap-v1").unwrap().unwrap();
    let replayed: crate::models::RouteResponse = serde_json::from_str(&stored.payload).unwrap();
    assert_eq!(replayed.catalog_version, "CAT-TEST-1");
    assert!(replayed
        .exclusions
        .iter()
        .any(|e| e.point_id == "POINT-C" && e.reason == "TEMPORARILY_CLOSED"));
    assert!(!replayed.candidates.iter().any(|c| c.point_id == "POINT-C"));

    let current = db.load_current_catalog().unwrap().unwrap();
    assert_eq!(current.catalog_version, "CAT-TEST-2");
    let resp_v2 = router::route(&current, &req, "snap-v2".to_string());
    assert!(resp_v2.candidates.iter().any(|c| c.point_id == "POINT-C"));
}

#[test]
fn batch_routing_returns_independent_responses() {
    let catalog = sample_catalog();
    let requests = vec![
        RouteRequest {
            origin_grid: [1, 1],
            service: "LEGAL_AID".to_string(),
            mobility: vec![Mobility::Wheelchair],
            communication: vec![],
            request_time: None,
        },
        RouteRequest {
            origin_grid: [2, 1],
            service: "NOTARY".to_string(),
            mobility: vec![],
            communication: vec![Communication::Hearing],
            request_time: None,
        },
    ];
    let results: Vec<_> = requests
        .iter()
        .map(|r| router::route(&catalog, r, Uuid::new_v4().to_string()))
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results[0].candidates.iter().all(|c| c.point_id != "POINT-B"));
    assert!(results[1].candidates.iter().any(|c| c.point_id == "POINT-B"));
}

#[tokio::test]
async fn concurrent_queries_see_a_single_catalog_version() {
    let db = Arc::new(Db::open_in_memory().unwrap());
    db.import_catalog(&sample_catalog()).unwrap();

    let mut handles = Vec::new();
    for _ in 0..16 {
        let db = db.clone();
        handles.push(tokio::spawn(async move {
            let catalog = db.load_current_catalog().unwrap().unwrap();
            let req = RouteRequest {
                origin_grid: [1, 1],
                service: "LEGAL_AID".to_string(),
                mobility: vec![],
                communication: vec![],
                request_time: None,
            };
            let resp = router::route(&catalog, &req, Uuid::new_v4().to_string());
            (catalog.catalog_version.clone(), resp)
        }));
    }
    for h in handles {
        let (version, resp) = h.await.unwrap();
        assert_eq!(version, "CAT-TEST-1");
        assert_eq!(resp.catalog_version, "CAT-TEST-1");
    }
}

#[test]
fn large_catalog_has_repeatable_timing_shape() {
    let mut catalog = sample_catalog();
    catalog.catalog_version = "CAT-LARGE".to_string();
    catalog.points = (0..5000)
        .map(|i| ServicePoint {
            id: format!("POINT-{:05}", i),
            grid: [i % 100, i / 100],
            services: BTreeSet::from(["LEGAL_AID".to_string()]),
            access: BTreeSet::new(),
            barrier_penalty: (i % 3) as i64,
        })
        .collect();
    catalog.closures = vec![];
    let req = RouteRequest {
        origin_grid: [50, 50],
        service: "LEGAL_AID".to_string(),
        mobility: vec![],
        communication: vec![],
        request_time: None,
    };
    let r1 = router::route(&catalog, &req, "a".to_string());
    let r2 = router::route(&catalog, &req, "b".to_string());
    assert_eq!(r1.candidates.len(), r2.candidates.len());
    let ids1: Vec<&str> = r1.candidates.iter().map(|c| c.point_id.as_str()).collect();
    let ids2: Vec<&str> = r2.candidates.iter().map(|c| c.point_id.as_str()).collect();
    assert_eq!(ids1, ids2);
    assert!(!r1.candidates.is_empty());
}
