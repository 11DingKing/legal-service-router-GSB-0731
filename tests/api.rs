//! End-to-end API tests against a real Axum app + real SQLite (temp files).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use legal_service_router::{api, db};
use serde_json::{json, Value};
use tower::ServiceExt;

fn fixture_catalog() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/materials/service-catalog.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

struct TestApp {
    app: axum::Router,
    _tmp: tempfile::TempDir,
}

fn new_app() -> TestApp {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("test.db").to_str().unwrap().to_string();
    let pool = db::open_pool(&db_path).unwrap();
    TestApp {
        app: api::build_app(pool),
        _tmp: tmp,
    }
}

async fn post_json(app: &axum::Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(body).unwrap()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let req = Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

async fn import(app: &axum::Router, catalog: &Value) -> (StatusCode, Value) {
    post_json(app, "/catalog/import", catalog).await
}

fn exclusion<'a>(outcome: &'a Value, point_id: &str) -> Option<&'a Value> {
    outcome["exclusions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["pointId"] == point_id)
}

fn reason_codes(e: &Value) -> Vec<&str> {
    e["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["code"].as_str().unwrap())
        .collect()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn import_activates_catalog_and_rejects_duplicate() {
    let t = new_app();
    let (status, body) = import(&t.app, &fixture_catalog()).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["version"], "CAT-2026-07-31");
    assert_eq!(body["active"], true);
    assert_eq!(body["points"], 3);

    let (status, body) = get(&t.app, "/catalog/active").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["version"], "CAT-2026-07-31");

    let (status, _) = import(&t.app, &fixture_catalog()).await;
    assert_eq!(status, StatusCode::CONFLICT, "duplicate version rejected");
}

#[tokio::test]
async fn hard_capability_gate_beats_shorter_distance() {
    let t = new_app();
    import(&t.app, &fixture_catalog()).await;

    // Wheelchair applicant standing next to POINT-B (distance 1).
    // POINT-B lacks STEP_FREE, so it must be excluded even though it is by
    // far the nearest; POINT-A (distance 3, STEP_FREE) wins.
    let req = json!({
        "serviceNeed": "LEGAL_AID",
        "mobility": "WHEELCHAIR",
        "origin": {"x": 2, "y": 0},
        "at": "2026-08-10T00:00:00Z"
    });
    let (status, out) = post_json(&t.app, "/route", &req).await;
    assert_eq!(status, StatusCode::OK);
    let candidates = out["candidates"].as_array().unwrap();
    assert_eq!(candidates[0]["pointId"], "POINT-A");
    assert_eq!(candidates[0]["totalCost"], 2);
    assert_eq!(candidates[0]["costBreakdown"]["distance"], 2);
    assert_eq!(candidates[0]["costBreakdown"]["barrierPenalty"], 0);

    let b = exclusion(&out, "POINT-B").expect("POINT-B must be excluded");
    assert_eq!(reason_codes(b), vec!["MISSING_REQUIRED_ACCESS"]);
    assert_eq!(b["reasons"][0]["capability"], "STEP_FREE");
}

#[tokio::test]
async fn closure_endpoints_are_half_open() {
    let t = new_app();
    import(&t.app, &fixture_catalog()).await;

    // POINT-C serves LEGAL_AID and has STEP_FREE; closure CLOSE-01 covers
    // [2026-08-02T00:00:00Z, 2026-08-04T00:00:00Z).
    let route_at = |at: String| {
        let app = t.app.clone();
        async move {
            let req = json!({
                "serviceNeed": "LEGAL_AID",
                "mobility": "WHEELCHAIR",
                "origin": {"x": 5, "y": 5},
                "at": at
            });
            post_json(&app, "/route", &req).await.1
        }
    };

    // Just before the closure: open.
    let out = route_at("2026-08-01T23:59:59Z".into()).await;
    assert!(exclusion(&out, "POINT-C").is_none(), "before from: open");
    // Exactly at `from`: closed (inclusive start).
    let out = route_at("2026-08-02T00:00:00Z".into()).await;
    let c = exclusion(&out, "POINT-C").expect("at from: closed");
    assert_eq!(reason_codes(c), vec!["TEMPORARILY_CLOSED"]);
    assert_eq!(c["reasons"][0]["eventId"], "CLOSE-01");
    assert_eq!(c["reasons"][0]["from"], "2026-08-02T00:00:00Z");
    assert_eq!(c["reasons"][0]["to"], "2026-08-04T00:00:00Z");
    // One second before `to`: still closed.
    let out = route_at("2026-08-03T23:59:59Z".into()).await;
    assert!(exclusion(&out, "POINT-C").is_some(), "inside window: closed");
    // Exactly at `to`: open again (exclusive end).
    let out = route_at("2026-08-04T00:00:00Z".into()).await;
    assert!(exclusion(&out, "POINT-C").is_none(), "at to: open again");
}

#[tokio::test]
async fn all_hard_capabilities_unsatisfiable_yields_reason_chain() {
    let t = new_app();
    import(&t.app, &fixture_catalog()).await;

    // NOTARY + WHEELCHAIR + SPEECH while POINT-C (the only point that could
    // satisfy every capability) is closed:
    //   A: no NOTARY.  B: NOTARY but no STEP_FREE / TEXT_COMMUNICATION.
    //   C: closed (still reports the closure even though it has everything).
    let req = json!({
        "serviceNeed": "NOTARY",
        "mobility": "WHEELCHAIR",
        "communication": ["SPEECH"],
        "origin": {"x": 0, "y": 0},
        "at": "2026-08-02T12:00:00Z"
    });
    let (status, out) = post_json(&t.app, "/route", &req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(out["candidates"].as_array().unwrap().len(), 0);
    assert_eq!(out["exclusions"].as_array().unwrap().len(), 3);

    assert_eq!(
        reason_codes(exclusion(&out, "POINT-A").unwrap()),
        vec!["SERVICE_UNAVAILABLE"]
    );
    let b = exclusion(&out, "POINT-B").unwrap();
    let caps: Vec<&str> = b["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["code"] == "MISSING_REQUIRED_ACCESS")
        .map(|r| r["capability"].as_str().unwrap())
        .collect();
    assert_eq!(caps, vec!["STEP_FREE", "TEXT_COMMUNICATION"]);
    assert_eq!(
        reason_codes(exclusion(&out, "POINT-C").unwrap()),
        vec!["TEMPORARILY_CLOSED"]
    );
}

#[tokio::test]
async fn equal_cost_candidates_tie_break_by_point_id() {
    let t = new_app();
    // Two STEP_FREE LEGAL_AID points at equal Manhattan cost, ids unordered.
    let catalog = json!({
        "catalogVersion": "CAT-TIE-1",
        "points": [
            {"id": "POINT-Z", "grid": [4, 0], "services": ["LEGAL_AID"], "access": ["STEP_FREE"], "barrierPenalty": 0},
            {"id": "POINT-Y", "grid": [0, 4], "services": ["LEGAL_AID"], "access": ["STEP_FREE"], "barrierPenalty": 0}
        ],
        "hardRequirements": {"WHEELCHAIR": "STEP_FREE"},
        "closures": []
    });
    import(&t.app, &catalog).await;
    let req = json!({
        "serviceNeed": "LEGAL_AID", "mobility": "WHEELCHAIR",
        "origin": {"x": 0, "y": 0}, "at": "2026-08-10T00:00:00Z"
    });
    let (_, out) = post_json(&t.app, "/route", &req).await;
    let ids: Vec<&str> = out["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["pointId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["POINT-Y", "POINT-Z"], "equal cost -> id ascending");
    assert_eq!(out["candidates"][0]["totalCost"], 4);
    assert_eq!(out["candidates"][1]["totalCost"], 4);
}

#[tokio::test]
async fn input_order_permutation_is_invariant() {
    // Importing points in a different order and sending communication needs
    // in a different order must not change ordering, reasons, or cost splits.
    let t1 = new_app();
    let t2 = new_app();
    import(&t1.app, &fixture_catalog()).await;
    let mut reversed = fixture_catalog();
    reversed["points"]
        .as_array_mut()
        .unwrap()
        .reverse();
    import(&t2.app, &reversed).await;

    let mk_req = |comm: [&str; 2]| json!({
        "serviceNeed": "LEGAL_AID",
        "mobility": "WHEELCHAIR",
        "communication": comm,
        "origin": {"x": 3, "y": 3},
        "at": "2026-08-02T12:00:00Z"
    });
    let (_, out1) = post_json(&t1.app, "/route", &mk_req(["HEARING", "SPEECH"])).await;
    let (_, out2) = post_json(&t2.app, "/route", &mk_req(["SPEECH", "HEARING"])).await;

    let strip = |o: &Value| {
        let mut o = o.clone();
        o.as_object_mut().unwrap().remove("snapshotId");
        o
    };
    assert_eq!(strip(&out1), strip(&out2));
    // And the exclusion/cost content is what we expect for this scenario.
    let cands: Vec<&str> = out1["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["pointId"].as_str().unwrap())
        .collect();
    assert_eq!(cands, Vec::<&str>::new(), "C closed, A/B lack SIGN_INTERPRETER");
}

/// A homebound applicant whose every entity option fails on hard conditions:
/// HOMEBOUND + HEARING + SPEECH + LEGAL_AID during CLOSE-01 excludes A
/// (no SIGN_INTERPRETER), B (no TEXT_COMMUNICATION) and C (closed).
fn homebound_hard_blocked_req() -> Value {
    json!({
        "serviceNeed": "LEGAL_AID", "mobility": "HOMEBOUND",
        "communication": ["HEARING", "SPEECH"],
        "origin": {"x": 1, "y": 1}, "at": "2026-08-02T12:00:00Z"
    })
}

#[tokio::test]
async fn home_service_is_degradation_path_with_preserved_reason_chain() {
    let t = new_app();
    import(&t.app, &fixture_catalog()).await;

    // All entity points excluded on hard conditions -> HOME_SERVICE_REQUIRED,
    // and the entity reason chains are preserved verbatim.
    let (_, out) = post_json(&t.app, "/route", &homebound_hard_blocked_req()).await;
    assert_eq!(out["homeService"]["eligible"], true);
    assert_eq!(out["homeService"]["reason"], "HOME_SERVICE_REQUIRED");
    assert_eq!(out["homeService"]["booking"]["slotId"], "SLOT-AM");
    assert_eq!(out["homeService"]["booking"]["cost"], 2);
    assert_eq!(out["candidates"].as_array().unwrap().len(), 0);
    assert_eq!(
        reason_codes(exclusion(&out, "POINT-A").unwrap()),
        vec!["MISSING_REQUIRED_ACCESS"]
    );
    assert_eq!(
        exclusion(&out, "POINT-A").unwrap()["reasons"][0]["capability"],
        "SIGN_INTERPRETER"
    );
    assert_eq!(
        exclusion(&out, "POINT-B").unwrap()["reasons"][0]["capability"],
        "TEXT_COMMUNICATION"
    );
    assert_eq!(
        reason_codes(exclusion(&out, "POINT-C").unwrap()),
        vec!["TEMPORARILY_CLOSED"]
    );

    // Entity candidates exist -> home service path does NOT trigger.
    let req = json!({
        "serviceNeed": "LEGAL_AID", "mobility": "HOMEBOUND",
        "origin": {"x": 1, "y": 1}, "at": "2026-08-10T00:00:00Z"
    });
    let (_, out) = post_json(&t.app, "/route", &req).await;
    assert!(out["homeService"].is_null());
    assert!(out["candidates"].as_array().unwrap().len() > 0);

    // Service mismatch: NOTARY is not the allowed home service. Here B is
    // reachable, so candidates are returned and no home outcome appears.
    let req = json!({
        "serviceNeed": "NOTARY", "mobility": "HOMEBOUND",
        "origin": {"x": 2, "y": 1}, "at": "2026-08-10T00:00:00Z"
    });
    let (_, out) = post_json(&t.app, "/route", &req).await;
    assert!(out["homeService"].is_null());
    assert_eq!(out["candidates"][0]["pointId"], "POINT-B");

    // All points excluded but one of them only for SERVICE_UNAVAILABLE
    // (POINT-A does not offer NOTARY): not a hard-condition sweep, so the
    // degradation path stays closed and reports NOT_AVAILABLE.
    let req = json!({
        "serviceNeed": "NOTARY", "mobility": "HOMEBOUND",
        "communication": ["SPEECH"],
        "origin": {"x": 0, "y": 0}, "at": "2026-08-02T12:00:00Z"
    });
    let (_, out) = post_json(&t.app, "/route", &req).await;
    assert_eq!(out["candidates"].as_array().unwrap().len(), 0);
    assert_eq!(out["homeService"]["eligible"], false);
    assert_eq!(out["homeService"]["reason"], "HOME_SERVICE_NOT_AVAILABLE");
    assert!(out["homeService"]["booking"].is_null());
    assert_eq!(
        reason_codes(exclusion(&out, "POINT-A").unwrap()),
        vec!["SERVICE_UNAVAILABLE"]
    );
}

#[tokio::test]
async fn home_service_equal_cost_slots_booked_in_stable_order() {
    let t = new_app();
    import(&t.app, &fixture_catalog()).await;

    // SLOT-AM and SLOT-PM both cost 2 with capacity 1: the first request
    // must take SLOT-AM (cost asc, slot id asc), the second SLOT-PM, and
    // the third must find no capacity — without falling back to entity
    // points that failed hard accessibility gates.
    let (_, r1) = post_json(&t.app, "/route", &homebound_hard_blocked_req()).await;
    let (_, r2) = post_json(&t.app, "/route", &homebound_hard_blocked_req()).await;
    let (_, r3) = post_json(&t.app, "/route", &homebound_hard_blocked_req()).await;

    assert_eq!(r1["homeService"]["booking"]["slotId"], "SLOT-AM");
    assert_eq!(r2["homeService"]["booking"]["slotId"], "SLOT-PM");
    assert_ne!(
        r1["homeService"]["booking"]["bookingId"],
        r2["homeService"]["booking"]["bookingId"]
    );

    assert!(r3["homeService"]["booking"].is_null());
    assert_eq!(r3["homeService"]["reason"], "HOME_SERVICE_NO_CAPACITY");
    assert_eq!(r3["homeService"]["eligible"], true);
    assert_eq!(
        r3["candidates"].as_array().unwrap().len(),
        0,
        "no fallback to hard-condition-failing entity points"
    );
    // Reason chain still fully preserved on the no-capacity response.
    assert_eq!(
        reason_codes(exclusion(&r3, "POINT-C").unwrap()),
        vec!["TEMPORARILY_CLOSED"]
    );
}

#[tokio::test]
async fn home_service_capacity_exhausts_mid_batch() {
    let t = new_app();
    import(&t.app, &fixture_catalog()).await;

    let batch = json!({"requests": [
        homebound_hard_blocked_req(),
        homebound_hard_blocked_req(),
        homebound_hard_blocked_req()
    ]});
    let (status, out) = post_json(&t.app, "/route/batch", &batch).await;
    assert_eq!(status, StatusCode::OK);
    let snaps = out["snapshots"].as_array().unwrap();
    assert_eq!(snaps.len(), 3);
    assert_eq!(snaps[0]["homeService"]["booking"]["slotId"], "SLOT-AM");
    assert_eq!(snaps[1]["homeService"]["booking"]["slotId"], "SLOT-PM");
    assert!(snaps[2]["homeService"]["booking"].is_null());
    assert_eq!(snaps[2]["homeService"]["reason"], "HOME_SERVICE_NO_CAPACITY");
    for s in snaps {
        assert_eq!(s["catalogVersion"], "CAT-2026-07-31");
        assert_eq!(s["candidates"].as_array().unwrap().len(), 0);
    }
}

#[tokio::test]
async fn home_service_concurrent_booking_race_never_double_books() {
    let t = new_app();
    import(&t.app, &fixture_catalog()).await;

    let mut racers = tokio::task::JoinSet::new();
    for _ in 0..6 {
        let app = t.app.clone();
        let req = homebound_hard_blocked_req();
        racers.spawn(async move { post_json(&app, "/route", &req).await });
    }
    let mut am = 0;
    let mut pm = 0;
    let mut no_capacity = 0;
    let mut snapshot_ids = std::collections::HashSet::new();
    while let Some(res) = racers.join_next().await {
        let (status, out) = res.unwrap();
        assert_eq!(status, StatusCode::OK);
        snapshot_ids.insert(out["snapshotId"].as_str().unwrap().to_string());
        match out["homeService"]["booking"]["slotId"].as_str() {
            Some("SLOT-AM") => am += 1,
            Some("SLOT-PM") => pm += 1,
            None => {
                assert_eq!(out["homeService"]["reason"], "HOME_SERVICE_NO_CAPACITY");
                no_capacity += 1;
            }
            other => panic!("unexpected slot {other:?}"),
        }
        assert_eq!(out["candidates"].as_array().unwrap().len(), 0);
    }
    assert_eq!((am, pm, no_capacity), (1, 1, 4), "capacity 1 per slot, no double-booking");
    assert_eq!(snapshot_ids.len(), 6);
}

#[tokio::test]
async fn home_service_capacity_is_scoped_by_catalog_version() {
    let t = new_app();
    import(&t.app, &fixture_catalog()).await;

    // Exhaust SLOT-AM under v1.
    let (_, r1) = post_json(&t.app, "/route", &homebound_hard_blocked_req()).await;
    assert_eq!(r1["homeService"]["booking"]["slotId"], "SLOT-AM");

    // Hot reload to a new version with the same slots: capacity is fresh
    // (bookings are scoped per version) and the route cache was invalidated.
    let mut v2 = fixture_catalog();
    v2["catalogVersion"] = json!("CAT-2026-08-02");
    import(&t.app, &v2).await;
    let (_, r2) = post_json(&t.app, "/route", &homebound_hard_blocked_req()).await;
    assert_eq!(r2["catalogVersion"], "CAT-2026-08-02");
    assert_eq!(
        r2["homeService"]["booking"]["slotId"], "SLOT-AM",
        "new version has its own capacity ledger"
    );

    // Replaying the v1 snapshot still shows the v1 booking, untouched.
    let (_, replay) = get(
        &t.app,
        &format!("/snapshots/{}", r1["snapshotId"].as_str().unwrap()),
    )
    .await;
    assert_eq!(replay, r1);
    assert_eq!(replay["catalogVersion"], "CAT-2026-07-31");
    assert_eq!(replay["homeService"]["booking"]["slotId"], "SLOT-AM");
}

#[tokio::test]
async fn batch_route_uses_one_version_and_persists_each_snapshot() {
    let t = new_app();
    import(&t.app, &fixture_catalog()).await;
    let batch = json!({
        "requests": [
            {"serviceNeed": "LEGAL_AID", "mobility": "WHEELCHAIR",
             "origin": {"x": 0, "y": 0}, "at": "2026-08-10T00:00:00Z"},
            {"serviceNeed": "MEDIATION", "communication": ["HEARING"],
             "origin": {"x": 4, "y": 4}, "at": "2026-08-02T12:00:00Z"},
            {"serviceNeed": "NOTARY", "origin": {"x": 2, "y": 1},
             "at": "2026-08-10T00:00:00Z"}
        ]
    });
    let (status, out) = post_json(&t.app, "/route/batch", &batch).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(out["catalogVersion"], "CAT-2026-07-31");
    let snaps = out["snapshots"].as_array().unwrap();
    assert_eq!(snaps.len(), 3);
    for s in snaps {
        assert_eq!(s["catalogVersion"], "CAT-2026-07-31");
        let (st, replay) = get(&t.app, &format!("/snapshots/{}", s["snapshotId"].as_str().unwrap())).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(replay, *s, "replay must return the stored snapshot verbatim");
    }
}

#[tokio::test]
async fn hot_reload_keeps_requests_consistent_and_replays_immutable() {
    let t = new_app();
    import(&t.app, &fixture_catalog()).await;

    // Snapshot under v1 while POINT-C is closed.
    let req = json!({
        "serviceNeed": "LEGAL_AID", "mobility": "WHEELCHAIR",
        "origin": {"x": 5, "y": 5}, "at": "2026-08-02T12:00:00Z"
    });
    let (_, v1_out) = post_json(&t.app, "/route", &req).await;
    let snapshot_id = v1_out["snapshotId"].as_str().unwrap().to_string();
    assert_eq!(v1_out["catalogVersion"], "CAT-2026-07-31");
    assert!(exclusion(&v1_out, "POINT-C").is_some());

    // Concurrent reads while a new catalog version is imported.
    let mut v2 = fixture_catalog();
    v2["catalogVersion"] = json!("CAT-2026-08-02");
    v2["closures"] = json!([]); // closure lifted in the new version
    v2["capabilityEvents"] = json!([]); // degradation lifted too
    let import_app = t.app.clone();
    let importer = tokio::spawn(async move { import(&import_app, &v2).await });

    let mut readers = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let app = t.app.clone();
        let r = req.clone();
        readers.spawn(async move { post_json(&app, "/route", &r).await });
    }
    let (import_status, _) = importer.await.unwrap();
    assert_eq!(import_status, StatusCode::CREATED);

    let mut saw_v1 = false;
    let mut saw_v2 = false;
    while let Some(res) = readers.join_next().await {
        let (status, out) = res.unwrap();
        assert_eq!(status, StatusCode::OK);
        match out["catalogVersion"].as_str().unwrap() {
            "CAT-2026-07-31" => {
                saw_v1 = true;
                // A v1 view must be complete v1 data: POINT-C closed.
                assert!(exclusion(&out, "POINT-C").is_some());
            }
            "CAT-2026-08-02" => {
                saw_v2 = true;
                // A v2 view must be complete v2 data: closure lifted.
                assert!(exclusion(&out, "POINT-C").is_none());
            }
            other => panic!("response mixed an unknown version: {other}"),
        }
    }
    assert!(saw_v2, "post-import reads must see the new version");
    println!("hot reload consistency: saw_v1={saw_v1} saw_v2={saw_v2}");

    // Replaying the old snapshot still returns the v1 result, uncontaminated.
    let (status, replay) = get(&t.app, &format!("/snapshots/{snapshot_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay, v1_out);
    assert_eq!(replay["catalogVersion"], "CAT-2026-07-31");
    assert!(exclusion(&replay, "POINT-C").is_some());
}

#[tokio::test]
async fn degradation_window_and_overlap_priority() {
    let t = new_app();
    import(&t.app, &fixture_catalog()).await;

    // CLOSE-01 closes POINT-C on [08-02, 08-04); DEGRADE-01 removes
    // SIGN_INTERPRETER at POINT-C on [08-03, 08-05) — a partial overlap.
    let hearing_at = |at: &str| {
        let app = t.app.clone();
        let at = at.to_string();
        async move {
            let req = json!({
                "serviceNeed": "LEGAL_AID", "communication": ["HEARING"],
                "origin": {"x": 5, "y": 5}, "at": at
            });
            post_json(&app, "/route", &req).await.1
        }
    };

    // Closure only: closed reason, no degradation noise.
    let out = hearing_at("2026-08-02T12:00:00Z").await;
    let c = exclusion(&out, "POINT-C").unwrap();
    assert_eq!(reason_codes(c), vec!["TEMPORARILY_CLOSED"]);

    // Overlap begins exactly at the degradation `from` endpoint: the closure
    // still dominates, degradation must NOT appear in the chain.
    let out = hearing_at("2026-08-03T00:00:00Z").await;
    let c = exclusion(&out, "POINT-C").unwrap();
    assert_eq!(reason_codes(c), vec!["TEMPORARILY_CLOSED"]);
    assert!(!reason_codes(c).contains(&"CAPABILITY_DEGRADED"));

    // Mid-overlap: same dominance.
    let out = hearing_at("2026-08-03T12:00:00Z").await;
    assert_eq!(
        reason_codes(exclusion(&out, "POINT-C").unwrap()),
        vec!["TEMPORARILY_CLOSED"]
    );

    // Closure `to` endpoint: closure lifted, degradation alone now excludes.
    let out = hearing_at("2026-08-04T00:00:00Z").await;
    let c = exclusion(&out, "POINT-C").unwrap();
    assert_eq!(reason_codes(c), vec!["CAPABILITY_DEGRADED"]);
    assert_eq!(c["reasons"][0]["capability"], "SIGN_INTERPRETER");
    assert_eq!(c["reasons"][0]["eventId"], "DEGRADE-01");
    assert_eq!(c["reasons"][0]["from"], "2026-08-03T00:00:00Z");
    assert_eq!(c["reasons"][0]["to"], "2026-08-05T00:00:00Z");

    // Degradation `to` endpoint: normal again, POINT-C is a candidate.
    let out = hearing_at("2026-08-05T00:00:00Z").await;
    assert!(exclusion(&out, "POINT-C").is_none());
    assert_eq!(out["candidates"][0]["pointId"], "POINT-C");

    // An applicant who does not need the degraded capability is unaffected
    // even inside the degradation window.
    let req = json!({
        "serviceNeed": "LEGAL_AID", "mobility": "WHEELCHAIR",
        "origin": {"x": 5, "y": 5}, "at": "2026-08-04T12:00:00Z"
    });
    let (_, out) = post_json(&t.app, "/route", &req).await;
    assert!(exclusion(&out, "POINT-C").is_none());
    assert_eq!(out["candidates"][0]["pointId"], "POINT-C");

    // Outside every event window the result equals the pre-window baseline.
    let strip = |o: &Value| {
        let mut o = o.clone();
        o.as_object_mut().unwrap().remove("snapshotId");
        let req = o["request"].as_object().unwrap();
        let mut req = req.clone();
        req.remove("at");
        req.remove("atEpoch");
        o.as_object_mut().unwrap().insert("request".into(), Value::Object(req));
        o
    };
    let before = strip(&hearing_at("2026-08-01T12:00:00Z").await);
    let after = strip(&hearing_at("2026-08-10T12:00:00Z").await);
    assert_eq!(before, after, "outside event windows results must be unchanged");
}

#[tokio::test]
async fn batch_pinned_to_one_version_during_hot_reload() {
    let t = new_app();
    import(&t.app, &fixture_catalog()).await;

    let mut v2 = fixture_catalog();
    v2["catalogVersion"] = json!("CAT-2026-08-02");
    v2["closures"] = json!([]);
    v2["capabilityEvents"] = json!([]);
    let import_app = t.app.clone();
    let importer = tokio::spawn(async move { import(&import_app, &v2).await });

    let batch = json!({
        "requests": [
            {"serviceNeed": "LEGAL_AID", "mobility": "WHEELCHAIR",
             "origin": {"x": 5, "y": 5}, "at": "2026-08-02T12:00:00Z"},
            {"serviceNeed": "LEGAL_AID", "communication": ["HEARING"],
             "origin": {"x": 5, "y": 5}, "at": "2026-08-02T12:00:00Z"},
            {"serviceNeed": "MEDIATION", "origin": {"x": 0, "y": 0},
             "at": "2026-08-02T12:00:00Z"},
            {"serviceNeed": "NOTARY", "origin": {"x": 2, "y": 1},
             "at": "2026-08-02T12:00:00Z"}
        ]
    });

    let mut batches = tokio::task::JoinSet::new();
    for _ in 0..20 {
        let app = t.app.clone();
        let b = batch.clone();
        batches.spawn(async move { post_json(&app, "/route/batch", &b).await });
    }
    let (import_status, _) = importer.await.unwrap();
    assert_eq!(import_status, StatusCode::CREATED);

    let mut saw_v1 = false;
    let mut saw_v2 = false;
    while let Some(res) = batches.join_next().await {
        let (status, out) = res.unwrap();
        assert_eq!(status, StatusCode::OK);
        let version = out["catalogVersion"].as_str().unwrap();
        let snaps = out["snapshots"].as_array().unwrap();
        assert_eq!(snaps.len(), 4);
        for s in snaps {
            // Every snapshot in the batch is pinned to the batch's version.
            assert_eq!(
                s["catalogVersion"].as_str().unwrap(),
                version,
                "batch must not mix catalog versions"
            );
            let c = exclusion(s, "POINT-C");
            match version {
                "CAT-2026-07-31" => {
                    saw_v1 = true;
                    // v1 at 2026-08-02T12: POINT-C is closed for everyone.
                    assert!(c.is_some(), "v1 view must show POINT-C closed");
                }
                "CAT-2026-08-02" => {
                    saw_v2 = true;
                    // v2 lifted every event: HEARING applicant reaches C.
                    assert!(c.is_none(), "v2 view must show POINT-C open");
                }
                other => panic!("unknown version {other}"),
            }
        }
    }
    assert!(saw_v2);
    println!("batch pinning during reload: saw_v1={saw_v1} saw_v2={saw_v2}");
}

#[tokio::test]
async fn cache_hit_is_deterministic_and_invalidated_on_reload() {
    let t = new_app();
    import(&t.app, &fixture_catalog()).await;

    let req = json!({
        "serviceNeed": "LEGAL_AID", "mobility": "WHEELCHAIR",
        "communication": ["HEARING"],
        "origin": {"x": 2, "y": 0}, "at": "2026-08-02T12:00:00Z"
    });
    let strip = |o: &Value| {
        let mut o = o.clone();
        o.as_object_mut().unwrap().remove("snapshotId");
        o
    };

    // First call: cache miss. Second call: cache hit — identical content,
    // different snapshotId, and both snapshots are persisted independently.
    let (_, r1) = post_json(&t.app, "/route", &req).await;
    let (_, r2) = post_json(&t.app, "/route", &req).await;
    assert_ne!(r1["snapshotId"], r2["snapshotId"]);
    assert_eq!(strip(&r1), strip(&r2), "cache hit must be identical");
    let (st, replay1) = get(&t.app, &format!("/snapshots/{}", r1["snapshotId"].as_str().unwrap())).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(replay1, r1);

    // Hot reload invalidates the cache: the same request must not return
    // stale pre-import data.
    let mut v2 = fixture_catalog();
    v2["catalogVersion"] = json!("CAT-2026-08-02");
    v2["closures"] = json!([]);
    v2["capabilityEvents"] = json!([]);
    import(&t.app, &v2).await;
    let (_, r3) = post_json(&t.app, "/route", &req).await;
    assert_eq!(r3["catalogVersion"], "CAT-2026-08-02");
    assert!(
        exclusion(&r3, "POINT-C").is_none(),
        "post-reload result must reflect the new version"
    );
    assert_ne!(strip(&r1), strip(&r3));

    // Replaying the pre-reload snapshot still returns the old version's data.
    let (_, replay1) = get(&t.app, &format!("/snapshots/{}", r1["snapshotId"].as_str().unwrap())).await;
    assert_eq!(replay1["catalogVersion"], "CAT-2026-07-31");
    assert!(exclusion(&replay1, "POINT-C").is_some());
}

#[tokio::test]
async fn equal_total_cost_with_different_breakdowns() {
    let t = new_app();
    // Same totalCost (4) reached by different distance/penalty splits.
    let catalog = json!({
        "catalogVersion": "CAT-TIE-2",
        "points": [
            {"id": "POINT-N", "grid": [4, 0], "services": ["LEGAL_AID"], "access": ["STEP_FREE"], "barrierPenalty": 0},
            {"id": "POINT-M", "grid": [2, 0], "services": ["LEGAL_AID"], "access": ["STEP_FREE"], "barrierPenalty": 2}
        ],
        "hardRequirements": {"WHEELCHAIR": "STEP_FREE"},
        "closures": [],
        "capabilityEvents": []
    });
    import(&t.app, &catalog).await;
    let req = json!({
        "serviceNeed": "LEGAL_AID", "mobility": "WHEELCHAIR",
        "origin": {"x": 0, "y": 0}, "at": "2026-08-10T00:00:00Z"
    });
    let (_, out) = post_json(&t.app, "/route", &req).await;
    let cands = out["candidates"].as_array().unwrap();
    assert_eq!(cands.len(), 2);
    assert_eq!(cands[0]["pointId"], "POINT-M", "tie -> id ascending");
    assert_eq!(cands[0]["totalCost"], 4);
    assert_eq!(cands[0]["costBreakdown"], json!({"distance": 2, "barrierPenalty": 2}));
    assert_eq!(cands[1]["pointId"], "POINT-N");
    assert_eq!(cands[1]["costBreakdown"], json!({"distance": 4, "barrierPenalty": 0}));
}

#[tokio::test]
async fn unknown_needs_and_bad_timestamps_are_rejected() {
    let t = new_app();
    import(&t.app, &fixture_catalog()).await;
    let bad_mobility = json!({
        "serviceNeed": "LEGAL_AID", "mobility": "TELEPORT",
        "origin": {"x": 0, "y": 0}
    });
    let (status, _) = post_json(&t.app, "/route", &bad_mobility).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let bad_time = json!({
        "serviceNeed": "LEGAL_AID", "origin": {"x": 0, "y": 0}, "at": "tomorrow"
    });
    let (status, _) = post_json(&t.app, "/route", &bad_time).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn large_catalog_performance_record() {
    // Deterministic large catalog: 3000 points on a grid ring, rotating
    // services/access/penalties so results are reproducible across runs.
    let n = 3000_i64;
    let services = ["LEGAL_AID", "NOTARY", "MEDIATION"];
    let access_sets: Vec<Vec<&str>> = vec![
        vec!["STEP_FREE"],
        vec!["SIGN_INTERPRETER"],
        vec!["TEXT_COMMUNICATION"],
        vec!["STEP_FREE", "SIGN_INTERPRETER", "TEXT_COMMUNICATION"],
        vec![],
    ];
    let points: Vec<Value> = (0..n)
        .map(|i| {
            json!({
                "id": format!("P{:05}", i),
                "grid": [(i * 37) % 173 - 86, (i * 91) % 173 - 86],
                "services": [services[(i % 3) as usize]],
                "access": access_sets[(i % 5) as usize],
                "barrierPenalty": i % 4
            })
        })
        .collect();
    let catalog = json!({
        "catalogVersion": "CAT-PERF-1",
        "costFormula": "abs(originGridX-pointGridX)+abs(originGridY-pointGridY)+barrierPenalty",
        "points": points,
        "hardRequirements": {"WHEELCHAIR": "STEP_FREE", "HEARING": "SIGN_INTERPRETER", "SPEECH": "TEXT_COMMUNICATION"},
        "closures": [{"eventId": "C1", "pointId": "P00010", "from": "2026-08-02T00:00:00Z", "to": "2026-08-04T00:00:00Z"}]
    });

    let t = new_app();
    let import_start = std::time::Instant::now();
    let (status, _) = import(&t.app, &catalog).await;
    let import_ms = import_start.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(status, StatusCode::CREATED);

    // 100 rounds, each with a distinct deterministic origin: every round is
    // a route-cache miss, so this measures real routing cost.
    let rounds = 100_i64;
    let mut miss_latencies = Vec::with_capacity(rounds as usize);
    let mut first: Option<Value> = None;
    let first_origin = json!({"x": -30, "y": -30}); // matches round i = 0
    for i in 0..rounds {
        let req = json!({
            "serviceNeed": "LEGAL_AID", "mobility": "WHEELCHAIR",
            "communication": ["HEARING"],
            "origin": {"x": (i * 13) % 61 - 30, "y": (i * 29) % 61 - 30},
            "at": "2026-08-10T00:00:00Z"
        });
        let start = std::time::Instant::now();
        let (status, out) = post_json(&t.app, "/route", &req).await;
        miss_latencies.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(status, StatusCode::OK);
        if i == 0 {
            first = Some(out);
        }
    }

    // Repeating the first request hits the route cache: content must be
    // identical modulo the fresh snapshotId.
    let req0 = json!({
        "serviceNeed": "LEGAL_AID", "mobility": "WHEELCHAIR",
        "communication": ["HEARING"],
        "origin": first_origin, "at": "2026-08-10T00:00:00Z"
    });
    let hit_start = std::time::Instant::now();
    let (_, hit) = post_json(&t.app, "/route", &req0).await;
    let hit_ms = hit_start.elapsed().as_secs_f64() * 1000.0;
    let mut a = first.clone().unwrap();
    let mut b = hit.clone();
    for v in [&mut a, &mut b] {
        v.as_object_mut().unwrap().remove("snapshotId");
    }
    assert_eq!(a, b, "cache hit must reproduce the miss result exactly");

    miss_latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let avg: f64 = miss_latencies.iter().sum::<f64>() / miss_latencies.len() as f64;
    let p95 = miss_latencies[(miss_latencies.len() as f64 * 0.95) as usize];
    let max = *miss_latencies.last().unwrap();
    let out = first.unwrap();
    println!(
        "PERF_RECORD catalog_points={n} rounds={rounds} import_ms={import_ms:.1} \
         miss_avg_ms={avg:.2} miss_p95_ms={p95:.2} miss_max_ms={max:.2} hit_ms={hit_ms:.2} \
         candidates={} exclusions={}",
        out["candidates"].as_array().unwrap().len(),
        out["exclusions"].as_array().unwrap().len()
    );
    // Generous debug-build bound; the printed record is the real artifact.
    assert!(p95 < 2000.0, "p95 route latency {p95:.2}ms exceeded bound");
}
