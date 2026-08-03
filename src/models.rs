use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(rename = "catalogVersion")]
    pub catalog_version: String,
    #[serde(rename = "costFormula")]
    pub cost_formula: String,
    pub points: Vec<ServicePoint>,
    #[serde(rename = "hardRequirements")]
    pub hard_requirements: HardRequirements,
    #[serde(rename = "tieBreak")]
    pub tie_break: Vec<String>,
    pub closures: Vec<Closure>,
    #[serde(rename = "homeService")]
    pub home_service: HomeService,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServicePoint {
    pub id: String,
    pub grid: [i64; 2],
    pub services: BTreeSet<String>,
    pub access: BTreeSet<String>,
    #[serde(rename = "barrierPenalty")]
    pub barrier_penalty: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardRequirements {
    #[serde(rename = "WHEELCHAIR")]
    pub wheelchair: String,
    #[serde(rename = "HEARING")]
    pub hearing: String,
    #[serde(rename = "SPEECH")]
    pub speech: String,
}

impl HardRequirements {
    pub fn required_access_for(&self, mobility: &[Mobility], communication: &[Communication]) -> BTreeSet<String> {
        let mut required = BTreeSet::new();
        for m in mobility {
            match m {
                Mobility::Wheelchair => {
                    required.insert(self.wheelchair.clone());
                }
                Mobility::Homebound => {}
                Mobility::Ambulatory => {}
            }
        }
        for c in communication {
            match c {
                Communication::Hearing => {
                    required.insert(self.hearing.clone());
                }
                Communication::Speech => {
                    required.insert(self.speech.clone());
                }
                Communication::None => {}
            }
        }
        required
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Closure {
    #[serde(rename = "eventId")]
    pub event_id: String,
    #[serde(rename = "pointId")]
    pub point_id: String,
    pub from: chrono::DateTime<chrono::Utc>,
    pub to: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeService {
    #[serde(rename = "allowedService")]
    pub allowed_service: String,
    #[serde(rename = "allowedMobility")]
    pub allowed_mobility: BTreeSet<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mobility {
    #[serde(rename = "WHEELCHAIR")]
    Wheelchair,
    #[serde(rename = "HOMEBOUND")]
    Homebound,
    #[serde(rename = "AMBULATORY")]
    Ambulatory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Communication {
    #[serde(rename = "HEARING")]
    Hearing,
    #[serde(rename = "SPEECH")]
    Speech,
    #[serde(rename = "NONE")]
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteRequest {
    #[serde(rename = "originGrid")]
    pub origin_grid: [i64; 2],
    pub service: String,
    #[serde(default)]
    pub mobility: Vec<Mobility>,
    #[serde(default)]
    pub communication: Vec<Communication>,
    #[serde(rename = "requestTime")]
    pub request_time: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BatchRouteRequest {
    pub requests: Vec<RouteRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteResponse {
    #[serde(rename = "snapshotId")]
    pub snapshot_id: String,
    #[serde(rename = "catalogVersion")]
    pub catalog_version: String,
    pub candidates: Vec<Candidate>,
    pub exclusions: Vec<Exclusion>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    #[serde(rename = "pointId")]
    pub point_id: String,
    #[serde(rename = "totalCost")]
    pub total_cost: i64,
    pub distance: i64,
    #[serde(rename = "barrierPenalty")]
    pub barrier_penalty: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Exclusion {
    #[serde(rename = "pointId")]
    pub point_id: String,
    pub reason: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchRouteResponse {
    pub results: Vec<RouteResponse>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotRecord {
    pub id: String,
    #[serde(rename = "catalogVersion")]
    pub catalog_version: String,
    pub payload: String,
    #[serde(rename = "createdAt")]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CatalogSummary {
    #[serde(rename = "catalogVersion")]
    pub catalog_version: String,
    #[serde(rename = "pointCount")]
    pub point_count: usize,
    #[serde(rename = "closureCount")]
    pub closure_count: usize,
    #[serde(rename = "importedAt")]
    pub imported_at: chrono::DateTime<chrono::Utc>,
}
