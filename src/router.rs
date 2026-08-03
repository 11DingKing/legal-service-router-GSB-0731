use crate::models::{
    Candidate, Catalog, Exclusion, Mobility, RouteRequest, RouteResponse,
};
use chrono::Utc;

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

        let missing: Vec<&String> = required_access
            .iter()
            .filter(|cap| !point.access.contains(*cap))
            .collect();
        if !missing.is_empty() {
            let missing_names: Vec<&str> = missing.iter().map(|s| s.as_str()).collect();
            exclusions.push(Exclusion {
                point_id: point.id.clone(),
                reason: "MISSING_ACCESSIBILITY".to_string(),
                detail: format!(
                    "point lacks required accessibility capability: {}",
                    missing_names.join(", ")
                ),
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
            .find(|c| c.point_id == point.id && request_time >= c.from && request_time < c.to)
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

        let distance = (request.origin_grid[0] - point.grid[0]).abs()
            + (request.origin_grid[1] - point.grid[1]).abs();
        let total_cost = distance + point.barrier_penalty;
        candidates.push(Candidate {
            point_id: point.id.clone(),
            total_cost,
            distance,
            barrier_penalty: point.barrier_penalty,
        });
    }

    candidates.sort_by(|a, b| {
        a.total_cost
            .cmp(&b.total_cost)
            .then_with(|| a.point_id.cmp(&b.point_id))
    });

    exclusions.sort_by(|a, b| a.point_id.cmp(&b.point_id));

    RouteResponse {
        snapshot_id,
        catalog_version: catalog.catalog_version.clone(),
        candidates,
        exclusions,
    }
}
