use crate::models::{
    Candidate, CandidateKind, Catalog, Degradation, Exclusion, Mobility, Notice, RouteRequest,
    RouteResponse, HOME_SERVICE_REASON,
};
use chrono::Utc;
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct HomeServiceOption {
    pub slot_id: String,
    pub distance: i64,
    pub total_cost: i64,
}

pub fn evaluate(
    catalog: &Catalog,
    request: &RouteRequest,
) -> (Vec<Candidate>, Vec<Exclusion>, Vec<HomeServiceOption>, Vec<Notice>) {
    let request_time = request.request_time.unwrap_or_else(Utc::now);
    let required_access = catalog
        .hard_requirements
        .required_access_for(&request.mobility, &request.communication);

    let is_homebound = request.mobility.contains(&Mobility::Homebound);
    let home_service_eligible = is_homebound
        && request.service == catalog.home_service.allowed_service
        && catalog
            .home_service
            .allowed_mobility
            .contains("HOMEBOUND");

    let mut candidates = Vec::new();
    let mut exclusions = Vec::new();

    for point in &catalog.points {
        if !point.services.contains(&request.service) {
            exclusions.push(Exclusion {
                point_id: point.id.clone(),
                reason: "MISSING_SERVICE".to_string(),
                detail: format!("point does not offer service {}", request.service),
            });
            continue;
        }

        if let Some(closure) = catalog
            .closures
            .iter()
            .find(|c| c.point_id == point.id && in_window(request_time, c.from, c.to))
        {
            exclusions.push(Exclusion {
                point_id: point.id.clone(),
                reason: "TEMPORARILY_CLOSED".to_string(),
                detail: format!(
                    "closure event {} active from {} to {}",
                    closure.event_id, closure.from, closure.to
                ),
            });
            continue;
        }

        let active_degradations: Vec<&Degradation> = catalog
            .degradations
            .iter()
            .filter(|d| d.point_id == point.id && in_window(request_time, d.from, d.to))
            .collect();

        let mut effective_access: BTreeSet<&String> = point.access.iter().collect();
        let mut removed_caps: BTreeSet<String> = BTreeSet::new();
        for d in &active_degradations {
            for cap in &d.unavailable_access {
                if effective_access.remove(cap) {
                    removed_caps.insert(cap.clone());
                }
            }
        }

        let missing: Vec<&String> = required_access
            .iter()
            .filter(|cap| !effective_access.contains(*cap))
            .collect();
        if !missing.is_empty() {
            let missing_names: Vec<&str> = missing.iter().map(|s| s.as_str()).collect();
            let mut detail = format!(
                "point lacks required accessibility capability: {}",
                missing_names.join(", ")
            );
            if !removed_caps.is_empty() {
                let removed_names: Vec<&str> = removed_caps.iter().map(|s| s.as_str()).collect();
                detail.push_str(&format!(
                    " (removed by active degradation: {})",
                    removed_names.join(", ")
                ));
            }
            exclusions.push(Exclusion {
                point_id: point.id.clone(),
                reason: "MISSING_ACCESSIBILITY".to_string(),
                detail,
            });
            continue;
        }

        let effective_penalty = active_degradations
            .iter()
            .find_map(|d| d.barrier_penalty_override)
            .unwrap_or(point.barrier_penalty);

        let distance = (request.origin_grid[0] - point.grid[0]).abs()
            + (request.origin_grid[1] - point.grid[1]).abs();
        let total_cost = distance + effective_penalty;
        candidates.push(Candidate {
            point_id: point.id.clone(),
            kind: CandidateKind::Physical,
            total_cost,
            distance,
            barrier_penalty: effective_penalty,
        });
    }

    let mut home_options = Vec::new();
    let mut notices = Vec::new();

    if home_service_eligible {
        let offers_service: Vec<&crate::models::ServicePoint> = catalog
            .points
            .iter()
            .filter(|p| p.services.contains(&request.service))
            .collect();
        let hard_or_closure: BTreeSet<&str> = exclusions
            .iter()
            .filter(|e| {
                e.reason == "MISSING_ACCESSIBILITY" || e.reason == "TEMPORARILY_CLOSED"
            })
            .map(|e| e.point_id.as_str())
            .collect();

        let all_physical_excluded = !offers_service.is_empty()
            && offers_service
                .iter()
                .all(|p| hard_or_closure.contains(p.id.as_str()));

        if all_physical_excluded {
            notices.push(Notice {
                code: HOME_SERVICE_REASON.to_string(),
                detail: "all in-person points are unavailable due to accessibility gaps or temporary closure; home-service fallback engaged".to_string(),
            });
            for slot in &catalog.home_service.slots {
                let distance = (request.origin_grid[0] - slot.grid[0]).abs()
                    + (request.origin_grid[1] - slot.grid[1]).abs();
                home_options.push(HomeServiceOption {
                    slot_id: slot.slot_id.clone(),
                    distance,
                    total_cost: distance,
                });
            }
        }
    }

    candidates.sort_by(|a, b| {
        a.total_cost
            .cmp(&b.total_cost)
            .then_with(|| a.point_id.cmp(&b.point_id))
    });
    home_options.sort_by(|a, b| {
        a.total_cost
            .cmp(&b.total_cost)
            .then_with(|| a.slot_id.cmp(&b.slot_id))
    });
    exclusions.sort_by(|a, b| {
        a.point_id
            .cmp(&b.point_id)
            .then_with(|| a.reason.cmp(&b.reason))
    });

    (candidates, exclusions, home_options, notices)
}

pub fn build_response(
    catalog: &Catalog,
    candidates: Vec<Candidate>,
    exclusions: Vec<Exclusion>,
    home_options: Vec<HomeServiceOption>,
    mut notices: Vec<Notice>,
    snapshot_id: String,
    assigned_slot: Option<&str>,
    no_capacity_reason: Option<&str>,
) -> RouteResponse {
    let mut final_candidates = candidates;

    if let Some(reason) = no_capacity_reason {
        notices.push(Notice {
            code: "NO_CAPACITY".to_string(),
            detail: reason.to_string(),
        });
    }

    if let Some(slot_id) = assigned_slot {
        if let Some(opt) = home_options.iter().find(|o| o.slot_id == slot_id) {
            final_candidates.push(Candidate {
                point_id: slot_id.to_string(),
                kind: CandidateKind::HomeService,
                total_cost: opt.total_cost,
                distance: opt.distance,
                barrier_penalty: 0,
            });
        }
        final_candidates.sort_by(|a, b| {
            a.total_cost
                .cmp(&b.total_cost)
                .then_with(|| a.point_id.cmp(&b.point_id))
        });
    }

    RouteResponse {
        snapshot_id,
        catalog_version: catalog.catalog_version.clone(),
        candidates: final_candidates,
        exclusions,
        notices,
    }
}

pub fn route(catalog: &Catalog, request: &RouteRequest, snapshot_id: String) -> RouteResponse {
    let (candidates, exclusions, home_options, notices) = evaluate(catalog, request);
    build_response(
        catalog,
        candidates,
        exclusions,
        home_options,
        notices,
        snapshot_id,
        None,
        None,
    )
}

pub fn route_with_assignment(
    catalog: &Catalog,
    request: &RouteRequest,
    snapshot_id: String,
    assigned_slot: Option<&str>,
    no_capacity_reason: Option<&str>,
) -> RouteResponse {
    let (candidates, exclusions, home_options, notices) = evaluate(catalog, request);
    build_response(
        catalog,
        candidates,
        exclusions,
        home_options,
        notices,
        snapshot_id,
        assigned_slot,
        no_capacity_reason,
    )
}

fn in_window(
    t: chrono::DateTime<chrono::Utc>,
    from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>,
) -> bool {
    t >= from && t < to
}
