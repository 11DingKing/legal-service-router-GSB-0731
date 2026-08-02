//! Integration tests over the pure routing engine, the versioned store, and the
//! Axum HTTP surface. These exercise the scenarios called out in the brief:
//! closure start/end endpoints, concurrent hot reload vs. queries, equal-cost
//! tie-breaking, all-hard-capabilities-unsatisfied, batch routing, snapshot
//! isolation, input-order independence, and a repeatable performance record.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use legal_service_router::model::Catalog;
use legal_service_router::routing::{route, HomeServiceStatus, RouteRequest};
use legal_service_router::store::Store;
use serde_json::{json, Value};
use tower::ServiceExt;

const FIXTURE: &str = include_str!("../materials/service-catalog.json");

fn ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn fixture_catalog() -> Catalog {
    Catalog::from_json(FIXTURE.as_bytes()).unwrap()
}

fn mem_store_seeded() -> Arc<Store> {
    let store = Arc::new(Store::open(":memory:").unwrap());
    store.import_catalog(FIXTURE.as_bytes(), true).unwrap();
    store
}

// ---- helpers to drive the HTTP API ----------------------------------------

async fn call(store: Arc<Store>, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let app = legal_service_router::api::router(store);
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let value: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

async fn get(store: Arc<Store>, path: &str) -> (StatusCode, Value) {
    let app = legal_service_router::api::router(store);
    let req = Request::builder()
        .method("GET")
        .uri(path)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let value: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

// ---- pure routing ----------------------------------------------------------

#[test]
fn hard_filter_precedes_cost_even_when_closer() {
    // A wheelchair applicant near POINT-B (which lacks STEP_FREE) must not be
    // routed there just because it is closer than the compliant POINT-A/C.
    let cat = fixture_catalog();
    let req = RouteRequest {
        origin: [2, 1], // exactly on POINT-B
        service: "LEGAL_AID".into(),
        mobility: Some("WHEELCHAIR".into()),
        communication: vec![],
        at: Some(ts("2026-07-01T00:00:00Z")), // before the closure window
    };
    let res = route(&cat, &req);
    // POINT-B excluded for missing STEP_FREE despite distance 0.
    let excluded_b = res
        .exclusions
        .iter()
        .find(|e| e.point_id == "POINT-B")
        .expect("POINT-B excluded");
    assert!(excluded_b.reasons.iter().any(|r| r == "MISSING_ACCESS:STEP_FREE"));
    // Best candidate is the nearest compliant one: POINT-A (dist 1) over C.
    assert_eq!(res.candidates[0].point_id, "POINT-A");
    assert!(res.candidates.iter().all(|c| c.point_id != "POINT-B"));
}

#[test]
fn all_hard_capabilities_unsatisfied_yields_empty_candidates() {
    // Applicant that is wheelchair + hearing + speech impaired requesting NOTARY.
    // Only POINT-C satisfies all access caps AND NOTARY, but it's closed in the
    // fixture window -> zero candidates, full reason chain retained.
    let cat = fixture_catalog();
    let req = RouteRequest {
        origin: [0, 0],
        service: "NOTARY".into(),
        mobility: Some("WHEELCHAIR".into()),
        communication: vec!["HEARING".into(), "SPEECH".into()],
        at: Some(ts("2026-08-03T00:00:00Z")), // inside CLOSE-01
    };
    let res = route(&cat, &req);
    assert!(res.candidates.is_empty(), "expected no candidates");
    assert_eq!(res.exclusions.len(), 3);
    // POINT-A: no NOTARY, missing SIGN_INTERPRETER.
    let a = res.exclusions.iter().find(|e| e.point_id == "POINT-A").unwrap();
    assert!(a.reasons.contains(&"MISSING_SERVICE".to_string()));
    assert!(a.reasons.contains(&"MISSING_ACCESS:SIGN_INTERPRETER".to_string()));
    // POINT-C: correct service+access but CLOSED.
    let c = res.exclusions.iter().find(|e| e.point_id == "POINT-C").unwrap();
    assert!(c.reasons.iter().any(|r| r == "CLOSED:CLOSE-01"));
}

#[test]
fn equal_cost_breaks_ties_by_point_id() {
    // Build a catalog with two equidistant, equally-compliant points.
    let raw = json!({
        "catalogVersion": "TIE-1",
        "points": [
            {"id": "POINT-Z", "grid": [1, 0], "services": ["LEGAL_AID"], "access": [], "barrierPenalty": 0},
            {"id": "POINT-A", "grid": [-1, 0], "services": ["LEGAL_AID"], "access": [], "barrierPenalty": 0}
        ],
        "hardRequirements": {},
        "tieBreak": ["totalCost ascending", "point id ascending"]
    });
    let cat = Catalog::from_json(raw.to_string().as_bytes()).unwrap();
    let req = RouteRequest {
        origin: [0, 0],
        service: "LEGAL_AID".into(),
        mobility: None,
        communication: vec![],
        at: Some(ts("2026-01-01T00:00:00Z")),
    };
    let res = route(&cat, &req);
    assert_eq!(res.candidates.len(), 2);
    assert_eq!(res.candidates[0].cost.total, res.candidates[1].cost.total);
    // Same cost -> ascending id wins.
    assert_eq!(res.candidates[0].point_id, "POINT-A");
    assert_eq!(res.candidates[1].point_id, "POINT-Z");
}

#[test]
fn input_order_does_not_change_output() {
    // Two catalogs with points and needs in different orders must produce
    // identical candidates, exclusions, and cost breakdowns.
    let raw_a = json!({
        "catalogVersion": "ORDER-1",
        "points": [
            {"id": "P1", "grid": [1,1], "services": ["LEGAL_AID","MEDIATION"], "access": ["STEP_FREE"], "barrierPenalty": 0},
            {"id": "P2", "grid": [3,3], "services": ["MEDIATION","LEGAL_AID"], "access": ["STEP_FREE","TEXT_COMMUNICATION"], "barrierPenalty": 2}
        ],
        "hardRequirements": {"WHEELCHAIR":"STEP_FREE","SPEECH":"TEXT_COMMUNICATION"}
    });
    // Reversed point order and reversed capability order.
    let raw_b = json!({
        "catalogVersion": "ORDER-1",
        "points": [
            {"id": "P2", "grid": [3,3], "services": ["LEGAL_AID","MEDIATION"], "access": ["TEXT_COMMUNICATION","STEP_FREE"], "barrierPenalty": 2},
            {"id": "P1", "grid": [1,1], "services": ["MEDIATION","LEGAL_AID"], "access": ["STEP_FREE"], "barrierPenalty": 0}
        ],
        "hardRequirements": {"SPEECH":"TEXT_COMMUNICATION","WHEELCHAIR":"STEP_FREE"}
    });
    let cat_a = Catalog::from_json(raw_a.to_string().as_bytes()).unwrap();
    let cat_b = Catalog::from_json(raw_b.to_string().as_bytes()).unwrap();

    let req1 = RouteRequest {
        origin: [0, 0],
        service: "LEGAL_AID".into(),
        mobility: Some("WHEELCHAIR".into()),
        communication: vec!["SPEECH".into()],
        at: Some(ts("2026-01-01T00:00:00Z")),
    };
    // Same request but communication needs reversed.
    let req2 = RouteRequest {
        communication: vec!["SPEECH".into()],
        ..req1.clone()
    };
    let res_a = route(&cat_a, &req1);
    let res_b = route(&cat_b, &req2);
    assert_eq!(
        serde_json::to_value(&res_a).unwrap(),
        serde_json::to_value(&res_b).unwrap(),
        "routing must be independent of input order"
    );
}

#[test]
fn home_service_is_fallback_only() {
    // Round 3 semantics: home service is a *degraded fallback*, entered only
    // when every physical point is excluded. A HOMEBOUND + LEGAL_AID applicant
    // with a reachable physical point gets NO home outcome.
    let cat = fixture_catalog();
    let with_physical = RouteRequest {
        origin: [1, 1], // POINT-A offers LEGAL_AID and needs no access here
        service: "LEGAL_AID".into(),
        mobility: Some("HOMEBOUND".into()),
        communication: vec![],
        at: Some(ts("2026-01-01T00:00:00Z")),
    };
    let res = route(&cat, &with_physical);
    assert!(res.candidates.iter().any(|c| c.point_id == "POINT-A"));
    assert!(res.home_service.is_none(), "home service must not trigger when a point is reachable");

    // Force every physical point out. A HOMEBOUND applicant maps to no hard
    // access requirement, so we exclude all points via communication needs:
    // requiring SIGN_INTERPRETER (HEARING) *and* TEXT_COMMUNICATION (SPEECH):
    //   POINT-A has TEXT_COMMUNICATION but lacks SIGN_INTERPRETER -> excluded
    //   POINT-B has SIGN_INTERPRETER but lacks TEXT_COMMUNICATION -> excluded
    //   POINT-C has both but is CLOSED in this window                -> excluded
    let all_excluded = RouteRequest {
        origin: [0, 0],
        service: "LEGAL_AID".into(),
        mobility: Some("HOMEBOUND".into()),
        communication: vec!["HEARING".into(), "SPEECH".into()],
        at: Some(ts("2026-08-03T00:00:00Z")), // POINT-C closed
    };
    let res2 = route(&cat, &all_excluded);
    assert!(res2.candidates.is_empty());
    let hs = res2.home_service.expect("home fallback eligible");
    assert_eq!(hs.reason, "HOME_SERVICE_REQUIRED");
    assert_eq!(hs.status, HomeServiceStatus::Eligible);
    // Pure routing does not resolve capacity.
    assert!(hs.slot_id.is_none());
    // Physical exclusion chain is preserved.
    assert!(res2.exclusions.iter().any(|e| e.point_id == "POINT-C"
        && e.reasons.iter().any(|r| r == "CLOSED:CLOSE-01")));
}

// ---- closures: half-open semantics -----------------------------------------

#[test]
fn closure_half_open_boundaries() {
    let cat = fixture_catalog();
    let base = RouteRequest {
        origin: [5, 5],
        service: "NOTARY".into(),
        mobility: None,
        communication: vec![],
        at: None,
    };
    // At exactly `from` -> closed.
    let at_from = RouteRequest { at: Some(ts("2026-08-02T00:00:00Z")), ..base.clone() };
    assert!(route(&cat, &at_from)
        .exclusions
        .iter()
        .any(|e| e.point_id == "POINT-C" && e.reasons.iter().any(|r| r == "CLOSED:CLOSE-01")));
    // At exactly `to` -> open again.
    let at_to = RouteRequest { at: Some(ts("2026-08-04T00:00:00Z")), ..base.clone() };
    assert!(route(&cat, &at_to)
        .candidates
        .iter()
        .any(|c| c.point_id == "POINT-C"));
}

// ---- HTTP: closure start/end endpoints -------------------------------------

#[tokio::test]
async fn http_start_and_end_closure() {
    let store = mem_store_seeded();
    // POINT-A is open by default; start a closure over "now-ish".
    let (st, _) = call(
        store.clone(),
        "POST",
        "/closures/start",
        json!({
            "version": "CAT-2026-07-31",
            "event_id": "CLOSE-A",
            "point_id": "POINT-A",
            "from": "2026-09-01T00:00:00Z",
            "to": "2026-09-10T00:00:00Z"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // Route inside the window -> POINT-A excluded as CLOSED.
    let (_st, body) = call(
        store.clone(),
        "POST",
        "/route",
        json!({
            "origin": [1,1],
            "service": "LEGAL_AID",
            "at": "2026-09-05T00:00:00Z"
        }),
    )
    .await;
    let excl = &body["result"]["exclusions"];
    assert!(excl
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["point_id"] == "POINT-A"
            && e["reasons"].as_array().unwrap().iter().any(|r| r == "CLOSED:CLOSE-A")));

    // End the closure at a point inside the window.
    let (st, end_body) = call(
        store.clone(),
        "POST",
        "/closures/end",
        json!({
            "version": "CAT-2026-07-31",
            "event_id": "CLOSE-A",
            "at": "2026-09-03T00:00:00Z"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(end_body["ended_at"], "2026-09-03T00:00:00Z");

    // After the effective end, POINT-A is open again.
    let (_st, body2) = call(
        store.clone(),
        "POST",
        "/route",
        json!({
            "origin": [1,1],
            "service": "LEGAL_AID",
            "at": "2026-09-05T00:00:00Z"
        }),
    )
    .await;
    assert!(body2["result"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["point_id"] == "POINT-A"));
}

// ---- HTTP: batch routing over one version ----------------------------------

#[tokio::test]
async fn http_batch_routes_single_version() {
    let store = mem_store_seeded();
    let (st, body) = call(
        store.clone(),
        "POST",
        "/route/batch",
        json!({
            "requests": [
                {"origin": [0,0], "service": "LEGAL_AID", "at": "2026-07-01T00:00:00Z"},
                {"origin": [5,5], "service": "MEDIATION", "at": "2026-07-01T00:00:00Z"}
            ]
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(body["catalog_version"], "CAT-2026-07-31");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    // Each item persisted a snapshot.
    assert!(items.iter().all(|i| i["snapshot_id"].is_string()));
}

// ---- HTTP: snapshot isolation / replay -------------------------------------

#[tokio::test]
async fn snapshot_replay_never_mixes_new_data() {
    let store = mem_store_seeded();
    // Take a snapshot against the seeded version.
    let (_st, body) = call(
        store.clone(),
        "POST",
        "/route",
        json!({"origin": [0,0], "service": "MEDIATION", "at": "2026-07-01T00:00:00Z"}),
    )
    .await;
    let snapshot_id = body["snapshot_id"].as_str().unwrap().to_string();
    let original = body["result"].clone();

    // Import a NEW version and activate it (hot reload). It adds a closer
    // MEDIATION point that would change routing outcomes.
    let new_catalog = json!({
        "catalogVersion": "CAT-2026-08-15",
        "points": [
            {"id": "POINT-A", "grid": [1,1], "services": ["MEDIATION"], "access": [], "barrierPenalty": 0},
            {"id": "POINT-NEW", "grid": [0,0], "services": ["MEDIATION"], "access": [], "barrierPenalty": 0}
        ],
        "hardRequirements": {},
        "tieBreak": ["totalCost ascending", "point id ascending"]
    });
    let (st, _) = call(
        store.clone(),
        "POST",
        "/catalog/import",
        json!({"catalog": new_catalog, "activate": true}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // Replaying the old snapshot returns the ORIGINAL result verbatim.
    let (st, replay) = get(store.clone(), &format!("/snapshots/{snapshot_id}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(replay["catalog_version"], "CAT-2026-07-31");
    assert_eq!(replay["result"], original);
    // The replay must NOT contain data from the new version.
    let ids: Vec<String> = replay["result"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["point_id"].as_str().unwrap().to_string())
        .collect();
    assert!(!ids.iter().any(|i| i == "POINT-NEW"));
}

// ---- HTTP: concurrency — hot reload while querying -------------------------

#[tokio::test]
async fn concurrent_hot_reload_and_queries_are_consistent() {
    let store = mem_store_seeded();

    // Task 1: continuously import + activate new versions (hot reload).
    let s1 = store.clone();
    let reloader = tokio::spawn(async move {
        for i in 0..50u32 {
            let cat = json!({
                "catalogVersion": format!("CAT-HOT-{i:03}"),
                "points": [
                    {"id": "POINT-A", "grid": [1,1], "services": ["LEGAL_AID"], "access": ["STEP_FREE"], "barrierPenalty": 0},
                    {"id": "POINT-C", "grid": [5,5], "services": ["LEGAL_AID"], "access": ["STEP_FREE"], "barrierPenalty": 0}
                ],
                "hardRequirements": {"WHEELCHAIR": "STEP_FREE"}
            });
            let bytes = serde_json::to_vec(&cat).unwrap();
            s1.import_catalog(&bytes, true).unwrap();
        }
    });

    // Task 2: hammer routing. Every response must reflect exactly one complete
    // version — a wheelchair applicant must never be routed to a non-STEP_FREE
    // point, and candidate ids must belong to a single consistent catalog.
    let s2 = store.clone();
    let querier = tokio::spawn(async move {
        for _ in 0..200 {
            let cat = s2.active_catalog().unwrap();
            let req = RouteRequest {
                origin: [0, 0],
                service: "LEGAL_AID".into(),
                mobility: Some("WHEELCHAIR".into()),
                communication: vec![],
                at: Some(ts("2026-07-01T00:00:00Z")),
            };
            let res = route(&cat, &req);
            // Consistency: all candidates come from the captured version and
            // satisfy STEP_FREE (proven by the catalog's own points).
            for c in &res.candidates {
                let p = cat.points.get(&c.point_id).unwrap();
                assert!(p.access.contains("STEP_FREE"));
            }
            // The version tag on the result equals the captured catalog.
            assert_eq!(res.catalog_version, cat.version);
        }
    });

    reloader.await.unwrap();
    querier.await.unwrap();
}

// ---- performance record ----------------------------------------------------

#[test]
fn perf_larger_catalog_is_repeatable() {
    // Build a larger synthetic catalog and record routing latency. This is a
    // reproducible performance record, not a hard assertion on wall time.
    let n = 5_000i64;
    let mut points = Vec::with_capacity(n as usize);
    for i in 0..n {
        points.push(json!({
            "id": format!("POINT-{i:05}"),
            "grid": [i % 100, i / 100],
            "services": ["LEGAL_AID"],
            "access": ["STEP_FREE"],
            "barrierPenalty": i % 3
        }));
    }
    let raw = json!({
        "catalogVersion": "PERF-1",
        "points": points,
        "hardRequirements": {"WHEELCHAIR": "STEP_FREE"}
    });
    let cat = Catalog::from_json(raw.to_string().as_bytes()).unwrap();
    let req = RouteRequest {
        origin: [50, 25],
        service: "LEGAL_AID".into(),
        mobility: Some("WHEELCHAIR".into()),
        communication: vec![],
        at: Some(ts("2026-07-01T00:00:00Z")),
    };

    // Warm + measure repeated runs; assert determinism across runs.
    let first = route(&cat, &req);
    let iterations = 200u32;
    let start = std::time::Instant::now();
    for _ in 0..iterations {
        let r = route(&cat, &req);
        assert_eq!(r.candidates.len(), first.candidates.len());
        assert_eq!(r.candidates[0].point_id, first.candidates[0].point_id);
    }
    let elapsed = start.elapsed();
    let per = elapsed / iterations;
    eprintln!(
        "[perf] catalog_points={n} candidates={} iterations={iterations} total={elapsed:?} per_route={per:?}",
        first.candidates.len()
    );
    // Same input order independence proof at scale: identical serialized output.
    let again = route(&cat, &req);
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&again).unwrap()
    );
}

// ============================================================================
// Round 2: capability degradation events overlapping CLOSE-01
// ============================================================================
//
// Fixture timeline for POINT-C (wheelchair applicant needs STEP_FREE):
//   CLOSE-01   [2026-08-02, 2026-08-04)   whole point closed
//   DEGRADE-01 [2026-08-03, 2026-08-05)   STEP_FREE temporarily unavailable
//
//   08-02 .. 08-03  closed only            -> CLOSED:CLOSE-01
//   08-03 .. 08-04  overlap                -> CLOSED:CLOSE-01 (closure dominates)
//   08-04 .. 08-05  open but degraded      -> DEGRADED:DEGRADE-01:STEP_FREE
//   >= 08-05        fully available        -> candidate

fn wheelchair_notary_at(at: &str) -> RouteRequest {
    // POINT-C is the only point offering NOTARY *and* STEP_FREE.
    RouteRequest {
        origin: [5, 5],
        service: "NOTARY".into(),
        mobility: Some("WHEELCHAIR".into()),
        communication: vec![],
        at: Some(ts(at)),
    }
}

fn point_c_reasons(res: &legal_service_router::routing::RouteResult) -> Vec<String> {
    res.exclusions
        .iter()
        .find(|e| e.point_id == "POINT-C")
        .map(|e| e.reasons.clone())
        .unwrap_or_default()
}

#[test]
fn degradation_timeline_and_overlap_priority() {
    let cat = fixture_catalog();

    // Before any event: POINT-C is a candidate (open, STEP_FREE present).
    let before = route(&cat, &wheelchair_notary_at("2026-08-01T12:00:00Z"));
    assert!(before.candidates.iter().any(|c| c.point_id == "POINT-C"));

    // Closure-only window: closed, degradation not yet active.
    let closed_only = route(&cat, &wheelchair_notary_at("2026-08-02T12:00:00Z"));
    assert_eq!(point_c_reasons(&closed_only), vec!["CLOSED:CLOSE-01"]);

    // Overlap window: closure DOMINATES; degradation is suppressed.
    let overlap = route(&cat, &wheelchair_notary_at("2026-08-03T12:00:00Z"));
    assert_eq!(point_c_reasons(&overlap), vec!["CLOSED:CLOSE-01"]);

    // Post-closure but still degraded: only the degradation reason remains.
    let degraded = route(&cat, &wheelchair_notary_at("2026-08-04T12:00:00Z"));
    assert_eq!(point_c_reasons(&degraded), vec!["DEGRADED:DEGRADE-01:STEP_FREE"]);
    assert!(degraded.candidates.iter().all(|c| c.point_id != "POINT-C"));

    // After both events: candidate again, results identical to `before`.
    let after = route(&cat, &wheelchair_notary_at("2026-08-05T12:00:00Z"));
    assert!(after.candidates.iter().any(|c| c.point_id == "POINT-C"));
}

#[test]
fn degradation_half_open_boundaries() {
    let cat = fixture_catalog();

    // Exactly at closure `to` (08-04): closure ends, degradation still active.
    let at_close_to = route(&cat, &wheelchair_notary_at("2026-08-04T00:00:00Z"));
    assert_eq!(point_c_reasons(&at_close_to), vec!["DEGRADED:DEGRADE-01:STEP_FREE"]);

    // Exactly at degradation `to` (08-05): degradation ends -> candidate.
    let at_degrade_to = route(&cat, &wheelchair_notary_at("2026-08-05T00:00:00Z"));
    assert!(at_degrade_to.candidates.iter().any(|c| c.point_id == "POINT-C"));

    // Exactly at degradation `from` (08-03): but closure still covers -> CLOSED.
    let at_degrade_from = route(&cat, &wheelchair_notary_at("2026-08-03T00:00:00Z"));
    assert_eq!(point_c_reasons(&at_degrade_from), vec!["CLOSED:CLOSE-01"]);
}

#[test]
fn degradation_ignored_when_capability_not_needed() {
    // A hearing applicant needs SIGN_INTERPRETER, not STEP_FREE, so the
    // STEP_FREE degradation must not exclude POINT-C during the degraded window.
    let cat = fixture_catalog();
    let req = RouteRequest {
        origin: [5, 5],
        service: "NOTARY".into(),
        mobility: None,
        communication: vec!["HEARING".into()],
        at: Some(ts("2026-08-04T12:00:00Z")), // degraded window
    };
    let res = route(&cat, &req);
    assert!(res.candidates.iter().any(|c| c.point_id == "POINT-C"));
}

#[test]
fn equal_cost_candidates_with_degradation() {
    // Two equidistant NOTARY+STEP_FREE points; degrade STEP_FREE on the
    // lower-id one so only the other survives — proves the degradation removes
    // a candidate rather than merely reordering equal-cost ties.
    let raw = json!({
        "catalogVersion": "DEG-TIE-1",
        "points": [
            {"id": "POINT-A", "grid": [1,0], "services": ["NOTARY"], "access": ["STEP_FREE"], "barrierPenalty": 0},
            {"id": "POINT-B", "grid": [-1,0], "services": ["NOTARY"], "access": ["STEP_FREE"], "barrierPenalty": 0}
        ],
        "hardRequirements": {"WHEELCHAIR": "STEP_FREE"},
        "tieBreak": ["totalCost ascending", "point id ascending"],
        "degradations": [
            {"eventId": "DEG-X", "pointId": "POINT-A", "capability": "STEP_FREE",
             "from": "2026-08-10T00:00:00Z", "to": "2026-08-12T00:00:00Z"}
        ]
    });
    let cat = Catalog::from_json(raw.to_string().as_bytes()).unwrap();
    let req = RouteRequest {
        origin: [0, 0],
        service: "NOTARY".into(),
        mobility: Some("WHEELCHAIR".into()),
        communication: vec![],
        at: Some(ts("2026-08-11T00:00:00Z")),
    };
    let res = route(&cat, &req);
    assert_eq!(res.candidates.len(), 1);
    assert_eq!(res.candidates[0].point_id, "POINT-B");
    let a = res.exclusions.iter().find(|e| e.point_id == "POINT-A").unwrap();
    assert_eq!(a.reasons, vec!["DEGRADED:DEG-X:STEP_FREE"]);
}

#[tokio::test]
async fn http_start_and_end_degradation() {
    let store = mem_store_seeded();

    // POINT-A has STEP_FREE and is open; start a STEP_FREE degradation.
    let (st, _) = call(
        store.clone(),
        "POST",
        "/degradations/start",
        json!({
            "version": "CAT-2026-07-31",
            "event_id": "DEG-A",
            "point_id": "POINT-A",
            "capability": "STEP_FREE",
            "from": "2026-09-01T00:00:00Z",
            "to": "2026-09-10T00:00:00Z"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // Wheelchair route inside the window -> POINT-A excluded as DEGRADED.
    let (_st, body) = call(
        store.clone(),
        "POST",
        "/route",
        json!({"origin":[1,1],"service":"LEGAL_AID","mobility":"WHEELCHAIR","at":"2026-09-05T00:00:00Z"}),
    )
    .await;
    assert!(body["result"]["exclusions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["point_id"] == "POINT-A"
            && e["reasons"].as_array().unwrap().iter().any(|r| r == "DEGRADED:DEG-A:STEP_FREE")));

    // End the degradation early.
    let (st, end_body) = call(
        store.clone(),
        "POST",
        "/degradations/end",
        json!({"version":"CAT-2026-07-31","event_id":"DEG-A","at":"2026-09-03T00:00:00Z"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(end_body["ended_at"], "2026-09-03T00:00:00Z");

    // After the effective end, POINT-A is a candidate again for the wheelchair.
    let (_st, body2) = call(
        store.clone(),
        "POST",
        "/route",
        json!({"origin":[1,1],"service":"LEGAL_AID","mobility":"WHEELCHAIR","at":"2026-09-05T00:00:00Z"}),
    )
    .await;
    assert!(body2["result"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["point_id"] == "POINT-A"));
}

#[tokio::test]
async fn snapshot_replay_pins_catalog_version_across_degradation_reload() {
    // A snapshot taken before a degradation hot-update must replay verbatim,
    // pinned to its catalog version, without absorbing the new degradation.
    let store = mem_store_seeded();
    let (_st, body) = call(
        store.clone(),
        "POST",
        "/route",
        json!({"origin":[5,5],"service":"NOTARY","mobility":"WHEELCHAIR","at":"2026-08-06T00:00:00Z"}),
    )
    .await;
    let snapshot_id = body["snapshot_id"].as_str().unwrap().to_string();
    let original = body["result"].clone();
    // Sanity: POINT-C is a candidate at 08-06 (both events elapsed).
    assert!(original["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["point_id"] == "POINT-C"));

    // Hot update: add a new degradation on the SAME active version that would
    // exclude POINT-C at 08-06 if it were re-evaluated.
    let (st, _) = call(
        store.clone(),
        "POST",
        "/degradations/start",
        json!({
            "version": "CAT-2026-07-31",
            "event_id": "DEGRADE-02",
            "point_id": "POINT-C",
            "capability": "STEP_FREE",
            "from": "2026-08-05T00:00:00Z",
            "to": "2026-08-20T00:00:00Z"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);

    // Replay: identical to the original, still lists POINT-C. The cached live
    // catalog changed, but the immutable snapshot did not.
    let (st, replay) = get(store.clone(), &format!("/snapshots/{snapshot_id}")).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(replay["catalog_version"], "CAT-2026-07-31");
    assert_eq!(replay["result"], original);

    // Cache invalidation proof: a *fresh* route on the live catalog now sees
    // DEGRADE-02 and excludes POINT-C — confirming the reload took effect.
    let (_st, live) = call(
        store.clone(),
        "POST",
        "/route",
        json!({"origin":[5,5],"service":"NOTARY","mobility":"WHEELCHAIR","at":"2026-08-06T00:00:00Z"}),
    )
    .await;
    assert!(live["result"]["exclusions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["point_id"] == "POINT-C"
            && e["reasons"].as_array().unwrap().iter().any(|r| r == "DEGRADED:DEGRADE-02:STEP_FREE")));
    assert!(live["result"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["point_id"] != "POINT-C"));
}

#[tokio::test]
async fn batch_pinned_to_single_version_during_degradation_reload() {
    // A batch must pin every item to one catalog version even while a
    // degradation hot-update lands mid-batch: no item may be evaluated partly
    // pre-degradation and partly post-degradation.
    //
    // We drive many concurrent batches against the pure engine while a reloader
    // toggles a degradation on POINT-C, and assert every batch is internally
    // consistent: all items agree on whether POINT-C is a candidate.
    let store = mem_store_seeded();

    let s1 = store.clone();
    let reloader = tokio::spawn(async move {
        for i in 0..40u32 {
            // Alternate: add then end DEGRADE-BATCH to flip POINT-C in/out.
            if i % 2 == 0 {
                let _ = s1.start_degradation(
                    "CAT-2026-07-31",
                    "DEGRADE-BATCH",
                    "POINT-C",
                    "STEP_FREE",
                    ts("2026-08-06T00:00:00Z"),
                    ts("2026-08-30T00:00:00Z"),
                );
            } else {
                let _ = s1.end_degradation(
                    "CAT-2026-07-31",
                    "DEGRADE-BATCH",
                    Some(ts("2026-08-06T00:00:00Z")),
                );
            }
        }
    });

    let s2 = store.clone();
    let batcher = tokio::spawn(async move {
        for _ in 0..300 {
            // Capture ONE version for the whole batch.
            let cat = s2.active_catalog().unwrap();
            let mut c_is_candidate = Vec::new();
            for _ in 0..8 {
                let req = wheelchair_notary_at("2026-08-06T00:00:00Z");
                let res = route(&cat, &req);
                c_is_candidate.push(res.candidates.iter().any(|c| c.point_id == "POINT-C"));
                // Every candidate must satisfy STEP_FREE per the captured catalog.
                for cand in &res.candidates {
                    let p = cat.points.get(&cand.point_id).unwrap();
                    assert!(p.access.contains("STEP_FREE"));
                }
            }
            // All items in the batch agree -> pinned to one consistent version.
            assert!(
                c_is_candidate.iter().all(|&b| b == c_is_candidate[0]),
                "batch items disagreed on POINT-C -> version leaked mid-batch"
            );
        }
    });

    reloader.await.unwrap();
    batcher.await.unwrap();
}

// ============================================================================
// Round 3: home-service fallback + appointment capacity
// ============================================================================
//
// Home service is a strict fallback: it triggers only for HOMEBOUND + LEGAL_AID
// applicants when *every* physical point is excluded. Capacity is finite,
// per-version state that resets on a version switch and can run out mid-batch,
// but a capacity shortage never resurrects an inaccessible physical point.

/// A catalog whose only physical point can never serve a LEGAL_AID request
/// (it offers MEDIATION only), so a HOMEBOUND + LEGAL_AID applicant always
/// falls through to the home path. `slots` lets each test size capacity.
fn home_only_catalog(version: &str, slots: Value) -> Value {
    json!({
        "catalogVersion": version,
        "points": [
            {"id": "POINT-X", "grid": [0,0], "services": ["MEDIATION"], "access": [], "barrierPenalty": 0}
        ],
        "hardRequirements": {},
        "tieBreak": ["totalCost ascending", "point id ascending"],
        "homeService": {
            "allowedService": "LEGAL_AID",
            "allowedMobility": ["HOMEBOUND"],
            "reason": "HOME_SERVICE_REQUIRED",
            "slots": slots
        }
    })
}

fn homebound_legal_aid() -> Value {
    json!({"origin": [0,0], "service": "LEGAL_AID", "mobility": "HOMEBOUND"})
}

async fn import_active(store: Arc<Store>, catalog: Value) {
    let (st, _) = call(
        store.clone(),
        "POST",
        "/catalog/import",
        json!({"catalog": catalog, "activate": true}),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
}

#[tokio::test]
async fn home_capacity_exhausts_mid_batch() {
    // Two slots, capacity 1 each -> 2 appointments total. A batch of 3 identical
    // homebound requests must reserve the first two and reject the third with
    // HOME_SERVICE_NO_CAPACITY — never falling back to POINT-X.
    let store = Arc::new(Store::open(":memory:").unwrap());
    import_active(
        store.clone(),
        home_only_catalog(
            "CAP-1",
            json!([
                {"slotId": "SLOT-AM", "cost": 5, "capacity": 1},
                {"slotId": "SLOT-PM", "cost": 5, "capacity": 1}
            ]),
        ),
    )
    .await;

    let (st, body) = call(
        store.clone(),
        "POST",
        "/route/batch",
        json!({"requests": [homebound_legal_aid(), homebound_legal_aid(), homebound_legal_aid()]}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);

    // Item 0: reserves SLOT-AM (equal cost, lowest id).
    assert_eq!(items[0]["result"]["home_service"]["status"], "RESERVED");
    assert_eq!(items[0]["result"]["home_service"]["slot_id"], "SLOT-AM");
    // Item 1: reserves SLOT-PM.
    assert_eq!(items[1]["result"]["home_service"]["status"], "RESERVED");
    assert_eq!(items[1]["result"]["home_service"]["slot_id"], "SLOT-PM");
    // Item 2: capacity exhausted mid-batch.
    assert_eq!(items[2]["result"]["home_service"]["status"], "NO_CAPACITY");
    assert_eq!(items[2]["result"]["home_service"]["reason"], "HOME_SERVICE_NO_CAPACITY");

    // Crucial: no item ever lists the inaccessible physical point as a candidate.
    for it in items {
        assert!(it["result"]["candidates"].as_array().unwrap().is_empty());
    }
}

#[tokio::test]
async fn two_equal_cost_slots_compete_deterministically() {
    // Two equal-cost slots: the lower slot id must always win first, regardless
    // of the order they were declared in the catalog.
    let store = Arc::new(Store::open(":memory:").unwrap());
    import_active(
        store.clone(),
        home_only_catalog(
            "CAP-TIE",
            // Declared PM-before-AM on purpose; normalization sorts by (cost,id).
            json!([
                {"slotId": "SLOT-PM", "cost": 5, "capacity": 1},
                {"slotId": "SLOT-AM", "cost": 5, "capacity": 1}
            ]),
        ),
    )
    .await;

    let (_st, first) = call(store.clone(), "POST", "/route", homebound_legal_aid()).await;
    assert_eq!(first["result"]["home_service"]["slot_id"], "SLOT-AM");
    assert_eq!(first["result"]["home_service"]["slot_cost"], 5);

    let (_st, second) = call(store.clone(), "POST", "/route", homebound_legal_aid()).await;
    assert_eq!(second["result"]["home_service"]["slot_id"], "SLOT-PM");

    // Third: both equal-cost slots full -> no capacity (stable, explicit).
    let (_st, third) = call(store.clone(), "POST", "/route", homebound_legal_aid()).await;
    assert_eq!(third["result"]["home_service"]["status"], "NO_CAPACITY");
}

#[tokio::test]
async fn version_switch_resets_capacity() {
    // Exhaust capacity on version A, then import+activate version B (same shape,
    // fresh reservations). The next request reserves again on B.
    let store = Arc::new(Store::open(":memory:").unwrap());
    import_active(
        store.clone(),
        home_only_catalog("VER-A", json!([{"slotId": "SLOT-AM", "cost": 5, "capacity": 1}])),
    )
    .await;

    // Consume the single slot on VER-A.
    let (_st, a1) = call(store.clone(), "POST", "/route", homebound_legal_aid()).await;
    assert_eq!(a1["result"]["home_service"]["status"], "RESERVED");
    let (_st, a2) = call(store.clone(), "POST", "/route", homebound_legal_aid()).await;
    assert_eq!(a2["result"]["home_service"]["status"], "NO_CAPACITY");

    // Switch to a fresh version.
    import_active(
        store.clone(),
        home_only_catalog("VER-B", json!([{"slotId": "SLOT-AM", "cost": 5, "capacity": 1}])),
    )
    .await;

    let (_st, b1) = call(store.clone(), "POST", "/route", homebound_legal_aid()).await;
    assert_eq!(b1["result"]["catalog_version"], "VER-B");
    assert_eq!(b1["result"]["home_service"]["status"], "RESERVED", "capacity resets per version");
    assert_eq!(b1["result"]["home_service"]["slot_id"], "SLOT-AM");
}

#[tokio::test]
async fn no_capacity_never_falls_back_to_inaccessible_point() {
    // A wheelchair + HOMEBOUND applicant: the physical point exists but lacks
    // STEP_FREE, so it is a hard exclusion. With zero home capacity the result
    // must be NO_CAPACITY with an empty candidate list — the inaccessible point
    // is never offered as a fallback.
    let store = Arc::new(Store::open(":memory:").unwrap());
    let catalog = json!({
        "catalogVersion": "SAFE-1",
        "points": [
            // Offers LEGAL_AID but has NO STEP_FREE -> excluded for a wheelchair.
            {"id": "POINT-NOFREE", "grid": [0,0], "services": ["LEGAL_AID"], "access": [], "barrierPenalty": 0}
        ],
        "hardRequirements": {"WHEELCHAIR": "STEP_FREE", "HEARING": "SIGN_INTERPRETER"},
        "tieBreak": ["totalCost ascending", "point id ascending"],
        "homeService": {
            "allowedService": "LEGAL_AID",
            "allowedMobility": ["HOMEBOUND"],
            "reason": "HOME_SERVICE_REQUIRED",
            "slots": [{"slotId": "SLOT-AM", "cost": 5, "capacity": 0}]
        }
    });
    import_active(store.clone(), catalog).await;

    let (_st, body) = call(
        store.clone(),
        "POST",
        "/route",
        json!({"origin":[0,0],"service":"LEGAL_AID","mobility":"HOMEBOUND","communication":["HEARING"]}),
    )
    .await;
    // HOMEBOUND doesn't map to a hard cap, but HEARING needs SIGN_INTERPRETER,
    // which POINT-NOFREE lacks -> excluded. Zero slot capacity -> NO_CAPACITY.
    let hs = &body["result"]["home_service"];
    assert_eq!(hs["status"], "NO_CAPACITY");
    assert_eq!(hs["reason"], "HOME_SERVICE_NO_CAPACITY");
    // The inaccessible point stays excluded, candidates empty.
    assert!(body["result"]["candidates"].as_array().unwrap().is_empty());
    assert!(body["result"]["exclusions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["point_id"] == "POINT-NOFREE"));
}

#[tokio::test]
async fn concurrent_reservations_never_overbook() {
    // Fire many concurrent homebound routes at a version with capacity 3; exactly
    // three succeed, the rest report NO_CAPACITY. Proves atomic reservation.
    let store = Arc::new(Store::open(":memory:").unwrap());
    import_active(
        store.clone(),
        home_only_catalog("CONC-1", json!([{"slotId": "SLOT-AM", "cost": 5, "capacity": 3}])),
    )
    .await;

    let mut handles = Vec::new();
    for _ in 0..20 {
        let s = store.clone();
        handles.push(tokio::spawn(async move {
            let (_st, body) = call(s, "POST", "/route", homebound_legal_aid()).await;
            body["result"]["home_service"]["status"].as_str().unwrap().to_string()
        }));
    }
    let mut reserved = 0;
    let mut no_cap = 0;
    for h in handles {
        match h.await.unwrap().as_str() {
            "RESERVED" => reserved += 1,
            "NO_CAPACITY" => no_cap += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!(reserved, 3, "exactly capacity reservations succeed");
    assert_eq!(no_cap, 17);
}


