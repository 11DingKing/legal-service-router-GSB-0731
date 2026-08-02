use serde_json::{json, Value};
use tempfile::TempDir;

async fn spawn_app() -> (String, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db").to_str().unwrap().to_string();
    let pool = legal_service_router::db::init_pool(&db_path).unwrap();
    let app = legal_service_router::api::build_router(pool);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), dir)
}

fn v1_catalog() -> Value {
    json!({
        "catalogVersion": "CAT-V1",
        "costFormula": "abs(originGridX-pointGridX)+abs(originGridY-pointGridY)+barrierPenalty",
        "points": [
            {"id": "POINT-A", "grid": [1, 1], "services": ["LEGAL_AID", "MEDIATION"], "access": ["STEP_FREE", "TEXT_COMMUNICATION"], "barrierPenalty": 0},
            {"id": "POINT-B", "grid": [2, 1], "services": ["LEGAL_AID", "NOTARY"], "access": ["SIGN_INTERPRETER"], "barrierPenalty": 1},
            {"id": "POINT-C", "grid": [5, 5], "services": ["LEGAL_AID", "NOTARY", "MEDIATION"], "access": ["STEP_FREE", "SIGN_INTERPRETER", "TEXT_COMMUNICATION"], "barrierPenalty": 0}
        ],
        "hardRequirements": {"WHEELCHAIR": "STEP_FREE", "HEARING": "SIGN_INTERPRETER", "SPEECH": "TEXT_COMMUNICATION"},
        "tieBreak": ["totalCost ascending", "point id ascending"],
        "closures": [{"eventId": "CLOSE-01", "pointId": "POINT-C", "from": "2026-08-02T00:00:00Z", "to": "2026-08-04T00:00:00Z"}],
        "homeService": {"allowedService": "LEGAL_AID", "allowedMobility": ["HOMEBOUND"], "reason": "HOME_SERVICE_REQUIRED"}
    })
}

fn v2_catalog() -> Value {
    let mut cat = v1_catalog();
    cat["catalogVersion"] = json!("CAT-V2");
    cat["points"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id": "POINT-D",
            "grid": [2, 2],
            "services": ["LEGAL_AID", "NOTARY", "MEDIATION"],
            "access": ["STEP_FREE", "SIGN_INTERPRETER", "TEXT_COMMUNICATION"],
            "barrierPenalty": 0
        }));
    cat["closures"] = json!([]);
    cat
}

async fn import(base: &str, catalog: &Value) {
    let resp = reqwest::Client::new()
        .post(format!("{base}/admin/import"))
        .header("content-type", "application/json")
        .body(catalog.to_string())
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "import failed: {}",
        resp.text().await.unwrap()
    );
}

async fn route(base: &str, body: &Value) -> Value {
    let resp = reqwest::Client::new()
        .post(format!("{base}/route"))
        .json(body)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert!(
        status.is_success(),
        "route failed ({status}): {text}"
    );
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn test_health_and_import_flow() {
    let (base, _dir) = spawn_app().await;
    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);

    import(&base, &v1_catalog()).await;

    let active: Value = reqwest::get(format!("{base}/admin/active"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(active["catalogVersion"], "CAT-V1");
}

#[tokio::test]
async fn test_route_filters_hard_capabilities_and_costs() {
    let (base, _dir) = spawn_app().await;
    import(&base, &v1_catalog()).await;

    let r = route(
        &base,
        &json!({
            "originGrid": [2, 2],
            "service": "LEGAL_AID",
            "queryTime": "2026-08-05T00:00:00Z"
        }),
    )
    .await;

    assert_eq!(r["catalogVersion"], "CAT-V1");
    let ids: Vec<&str> = r["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["pointId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["POINT-A", "POINT-B", "POINT-C"]);
    assert_eq!(r["candidates"][0]["totalCost"], 2);
    assert_eq!(r["candidates"][0]["costBreakdown"]["distance"], 2);
    assert_eq!(r["candidates"][1]["totalCost"], 2);
    assert_eq!(r["candidates"][1]["costBreakdown"]["distance"], 1);
    assert_eq!(r["candidates"][1]["costBreakdown"]["barrierPenalty"], 1);
}

#[tokio::test]
async fn test_wheelchair_hard_requirement_excludes_closer_point() {
    let (base, _dir) = spawn_app().await;
    import(&base, &v1_catalog()).await;

    let r = route(
        &base,
        &json!({
            "originGrid": [2, 2],
            "service": "LEGAL_AID",
            "mobility": ["WHEELCHAIR"],
            "queryTime": "2026-08-05T00:00:00Z"
        }),
    )
    .await;

    let ids: Vec<&str> = r["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["pointId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["POINT-A", "POINT-C"]);

    let b = r["excluded"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["pointId"] == "POINT-B")
        .unwrap();
    assert_eq!(b["reasons"][0]["code"], "MISSING_ACCESS");
    assert_eq!(b["reasons"][0]["required"], "STEP_FREE");
}

#[tokio::test]
async fn test_closure_start_and_end_boundaries() {
    let (base, _dir) = spawn_app().await;
    import(&base, &v1_catalog()).await;

    let at_start = route(
        &base,
        &json!({"originGrid": [2, 2], "service": "LEGAL_AID", "queryTime": "2026-08-02T00:00:00Z"}),
    )
    .await;
    let c_excluded = at_start["excluded"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["pointId"] == "POINT-C"
            && e["reasons"][0]["code"] == "TEMPORARILY_CLOSED");
    assert!(c_excluded, "POINT-C should be closed at the start instant");

    let at_end = route(
        &base,
        &json!({"originGrid": [2, 2], "service": "LEGAL_AID", "queryTime": "2026-08-04T00:00:00Z"}),
    )
    .await;
    let c_included = at_end["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["pointId"] == "POINT-C");
    assert!(c_included, "POINT-C should be open at the end instant");
}

#[tokio::test]
async fn test_no_candidates_all_hard_capabilities_unsatisfied() {
    let (base, _dir) = spawn_app().await;
    import(&base, &v1_catalog()).await;

    let r = route(
        &base,
        &json!({
            "originGrid": [2, 2],
            "service": "NOTARY",
            "mobility": ["WHEELCHAIR"],
            "communication": ["HEARING"],
            "queryTime": "2026-08-02T12:00:00Z"
        }),
    )
    .await;

    assert!(r["candidates"].as_array().unwrap().is_empty());
    assert_eq!(r["excluded"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn test_snapshot_replay_is_immutable_across_versions() {
    let (base, _dir) = spawn_app().await;
    import(&base, &v1_catalog()).await;

    let r1 = route(
        &base,
        &json!({
            "originGrid": [2, 2],
            "service": "LEGAL_AID",
            "queryTime": "2026-08-05T00:00:00Z"
        }),
    )
    .await;
    let snapshot_id = r1["snapshotId"].as_str().unwrap().to_string();
    let v1_ids: Vec<&str> = r1["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["pointId"].as_str().unwrap())
        .collect();
    assert_eq!(v1_ids, vec!["POINT-A", "POINT-B", "POINT-C"]);

    import(&base, &v2_catalog()).await;

    let replayed: Value = reqwest::Client::new()
        .get(format!("{base}/snapshots/{snapshot_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(replayed["catalogVersion"], "CAT-V1");
    let replayed_ids: Vec<&str> = replayed["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["pointId"].as_str().unwrap())
        .collect();
    assert_eq!(replayed_ids, vec!["POINT-A", "POINT-B", "POINT-C"]);
    assert!(
        !replayed_ids.contains(&"POINT-D"),
        "old snapshot must not include V2 data"
    );

    let r2 = route(
        &base,
        &json!({
            "originGrid": [2, 2],
            "service": "LEGAL_AID",
            "queryTime": "2026-08-05T00:00:00Z"
        }),
    )
    .await;
    assert_eq!(r2["catalogVersion"], "CAT-V2");
    assert_eq!(r2["candidates"][0]["pointId"], "POINT-D");
    assert_eq!(r2["candidates"][0]["totalCost"], 0);
}

#[tokio::test]
async fn test_batch_routing_pins_single_version() {
    let (base, _dir) = spawn_app().await;
    import(&base, &v1_catalog()).await;

    let body = json!({
        "requests": [
            {"originGrid": [2, 2], "service": "LEGAL_AID", "queryTime": "2026-08-05T00:00:00Z"},
            {"originGrid": [5, 5], "service": "NOTARY", "mobility": ["WHEELCHAIR"], "queryTime": "2026-08-05T00:00:00Z"},
            {"originGrid": [1, 1], "service": "MEDIATION", "communication": ["SPEECH"], "queryTime": "2026-08-05T00:00:00Z"}
        ]
    });

    let resp = reqwest::Client::new()
        .post(format!("{base}/route/batch"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let batch: Value = resp.json().await.unwrap();

    assert_eq!(batch["catalogVersion"], "CAT-V1");
    assert_eq!(batch["results"].as_array().unwrap().len(), 3);
    for item in batch["results"].as_array().unwrap() {
        assert_eq!(item["response"]["catalogVersion"], "CAT-V1");
        assert!(item["snapshotId"].is_string());
    }

    let third = &batch["results"][2]["response"];
    let ids: Vec<&str> = third["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["pointId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["POINT-A", "POINT-C"]);
}

#[tokio::test]
async fn test_batch_input_order_independence() {
    let (base, _dir) = spawn_app().await;
    import(&base, &v1_catalog()).await;

    let req_a = json!({"originGrid": [2, 2], "service": "LEGAL_AID", "mobility": ["WHEELCHAIR"], "queryTime": "2026-08-05T00:00:00Z"});
    let req_b = json!({"originGrid": [5, 5], "service": "NOTARY", "queryTime": "2026-08-05T00:00:00Z"});

    let batch1 = json!({"requests": [req_a, req_b]});
    let batch2 = json!({"requests": [req_b, req_a]});

    async fn run(base: &str, b: Value) -> Value {
        reqwest::Client::new()
            .post(format!("{base}/route/batch"))
            .json(&b)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    let r1 = run(&base, batch1).await;
    let r2 = run(&base, batch2).await;

    let res1_a = &r1["results"][0]["response"];
    let res2_a = r2["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["response"]["service"] == "LEGAL_AID")
        .unwrap();
    let res2_a = &res2_a["response"];

    let ids1: Vec<&str> = res1_a["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["pointId"].as_str().unwrap())
        .collect();
    let ids2: Vec<&str> = res2_a["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["pointId"].as_str().unwrap())
        .collect();
    assert_eq!(ids1, ids2);
    assert_eq!(res1_a["candidates"], res2_a["candidates"]);
    assert_eq!(res1_a["excluded"], res2_a["excluded"]);
}

#[tokio::test]
async fn test_concurrent_queries_during_hot_reload() {
    let (base, _dir) = spawn_app().await;
    import(&base, &v1_catalog()).await;

    let pre = route(
        &base,
        &json!({"originGrid": [2, 2], "service": "LEGAL_AID", "queryTime": "2026-08-05T00:00:00Z"}),
    )
    .await;
    let v1_snapshot = pre["snapshotId"].as_str().unwrap().to_string();

    let client = reqwest::Client::new();
    let mut handles = Vec::new();
    for _ in 0..60 {
        let c = client.clone();
        let b = base.clone();
        handles.push(tokio::spawn(async move {
            c.post(format!("{b}/route"))
                .json(&json!({"originGrid": [2, 2], "service": "LEGAL_AID", "queryTime": "2026-08-05T00:00:00Z"}))
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }));
    }

    import(&base, &v2_catalog()).await;

    let mut v1_count = 0;
    let mut v2_count = 0;
    for h in handles {
        let r = h.await.unwrap();
        let version = r["catalogVersion"].as_str().unwrap();
        let ids: Vec<&str> = r["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["pointId"].as_str().unwrap())
            .collect();
        match version {
            "CAT-V1" => {
                v1_count += 1;
                assert!(!ids.contains(&"POINT-D"), "V1 response leaked V2 point");
            }
            "CAT-V2" => {
                v2_count += 1;
                assert!(ids.contains(&"POINT-D"), "V2 response missing V2 point");
                assert_eq!(ids[0], "POINT-D");
            }
            other => panic!("unexpected version {other}"),
        }
    }
    assert!(v1_count + v2_count == 60, "all requests must complete");

    let replayed: Value = client
        .get(format!("{base}/snapshots/{v1_snapshot}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replayed["catalogVersion"], "CAT-V1");
    let replayed_ids: Vec<&str> = replayed["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["pointId"].as_str().unwrap())
        .collect();
    assert!(!replayed_ids.contains(&"POINT-D"));
}

#[tokio::test]
async fn test_duplicate_import_version_conflicts() {
    let (base, _dir) = spawn_app().await;
    import(&base, &v1_catalog()).await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/admin/import"))
        .json(&v1_catalog())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409);
}

#[tokio::test]
async fn test_home_service_flag() {
    let (base, _dir) = spawn_app().await;
    import(&base, &v1_catalog()).await;

    let r = route(
        &base,
        &json!({
            "originGrid": [2, 2],
            "service": "LEGAL_AID",
            "mobility": ["HOMEBOUND"],
            "queryTime": "2026-08-05T00:00:00Z"
        }),
    )
    .await;
    assert_eq!(r["homeService"]["eligible"], true);
    assert_eq!(r["homeService"]["reason"], "HOME_SERVICE_REQUIRED");
}

#[tokio::test]
async fn test_large_catalog_performance_is_repeatable() {
    let (base, _dir) = spawn_app().await;

    let mut points = Vec::new();
    let grid_w = 20;
    let grid_h = 20;
    for i in 0..(grid_w * grid_h) {
        let x = i % grid_w;
        let y = i / grid_w;
        points.push(json!({
            "id": format!("POINT-{i:04}"),
            "grid": [x, y],
            "services": ["LEGAL_AID", "NOTARY", "MEDIATION"],
            "access": ["STEP_FREE", "SIGN_INTERPRETER", "TEXT_COMMUNICATION"],
            "barrierPenalty": i % 3
        }));
    }
    let big = json!({
        "catalogVersion": "CAT-BIG",
        "costFormula": "abs(originGridX-pointGridX)+abs(originGridY-pointGridY)+barrierPenalty",
        "points": points,
        "hardRequirements": {"WHEELCHAIR": "STEP_FREE", "HEARING": "SIGN_INTERPRETER", "SPEECH": "TEXT_COMMUNICATION"},
        "tieBreak": ["totalCost ascending", "point id ascending"],
        "closures": [],
        "homeService": {"allowedService": "LEGAL_AID", "allowedMobility": ["HOMEBOUND"], "reason": "HOME_SERVICE_REQUIRED"}
    });
    import(&base, &big).await;

    let query_count = 50;
    let run_queries = || async {
        let mut out = Vec::new();
        for i in 0..query_count {
            let ox = (i * 7) % grid_w;
            let oy = (i * 13) % grid_h;
            let r = route(
                &base,
                &json!({"originGrid": [ox, oy], "service": "LEGAL_AID", "queryTime": "2026-08-05T00:00:00Z"}),
            )
            .await;
            out.push(r);
        }
        out
    };

    let start = std::time::Instant::now();
    let results1 = run_queries().await;
    let first_elapsed = start.elapsed();

    let start = std::time::Instant::now();
    let results2 = run_queries().await;
    let second_elapsed = start.elapsed();

    assert_eq!(results1.len(), query_count as usize);
    assert_eq!(results2.len(), query_count as usize);
    for (a, b) in results1.iter().zip(results2.iter()) {
        assert_eq!(a["candidates"], b["candidates"]);
        assert_eq!(a["excluded"], b["excluded"]);
        assert_eq!(a["candidates"][0]["totalCost"], b["candidates"][0]["totalCost"]);
    }

    eprintln!(
        "{}-point catalog ({} queries): first run {:?}, second run {:?}",
        grid_w * grid_h, query_count, first_elapsed, second_elapsed
    );
    assert!(
        first_elapsed.as_secs() < 10,
        "first run too slow: {first_elapsed:?}"
    );
    assert!(
        second_elapsed.as_secs() < 10,
        "second run too slow: {second_elapsed:?}"
    );
}
