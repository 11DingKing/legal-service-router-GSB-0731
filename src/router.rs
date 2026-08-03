use crate::models::{
    Candidate, Catalog, Degradation, Exclusion, Mobility, RouteRequest, RouteResponse,
};
use chrono::Utc;
use std::collections::BTreeSet;

pub fn route(catalog: &Catalog, request: &RouteRequest, snapshot_id: String) -> RouteResponse {
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

        if home_service_eligible {
            exclusions.push(Exclusion {
                point_id: point.id.clone(),
                reason: catalog.home_service.reason.clone(),
                detail: "applicant is eligible for home service; in-person points excluded"
                    .to_string(),
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
            total_cost,
            distance,
            barrier_penalty: effective_penalty,
        });
    }

    candidates.sort_by(|a, b| {
        a.total_cost
            .cmp(&b.total_cost)
            .then_with(|| a.point_id.cmp(&b.point_id))
    });

    exclusions.sort_by(|a, b| {
        a.point_id
            .cmp(&b.point_id)
            .then_with(|| a.reason.cmp(&b.reason))
    });

    RouteResponse {
        snapshot_id,
        catalog_version: catalog.catalog_version.clone(),
        candidates,
        exclusions,
    }
}

fn in_window(
    t: chrono::DateTime<chrono::Utc>,
    from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>,
) -> bool {
    t >= from && t < to
}
