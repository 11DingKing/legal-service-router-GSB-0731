use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Catalog {
    #[serde(rename = "catalogVersion")]
    pub catalog_version: String,
    #[serde(rename = "costFormula")]
    pub cost_formula: String,
    pub points: Vec<Point>,
    #[serde(rename = "hardRequirements")]
    pub hard_requirements: HashMap<String, String>,
    #[serde(rename = "tieBreak")]
    pub tie_break: Vec<String>,
    pub closures: Vec<Closure>,
    #[serde(rename = "homeService")]
    pub home_service: HomeServiceConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Point {
    pub id: String,
    pub grid: [i32; 2],
    pub services: Vec<String>,
    pub access: Vec<String>,
    #[serde(rename = "barrierPenalty")]
    pub barrier_penalty: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Closure {
    #[serde(rename = "eventId")]
    pub event_id: String,
    #[serde(rename = "pointId")]
    pub point_id: String,
    #[serde(deserialize_with = "parse_time", serialize_with = "serialize_time")]
    pub from: DateTime<Utc>,
    #[serde(deserialize_with = "parse_time", serialize_with = "serialize_time")]
    pub to: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HomeServiceConfig {
    #[serde(rename = "allowedService")]
    pub allowed_service: String,
    #[serde(rename = "allowedMobility")]
    pub allowed_mobility: Vec<String>,
    pub reason: String,
}

fn parse_time<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    parse_rfc3339(&s).map_err(serde::de::Error::custom)
}

fn serialize_time<S>(dt: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(&dt.to_rfc3339())
}

pub fn parse_rfc3339(s: &str) -> AppResult<DateTime<Utc>> {
    let dt = DateTime::parse_from_rfc3339(s.trim())?;
    Ok(dt.with_timezone(&Utc))
}

pub fn parse_catalog(json: &str) -> AppResult<Catalog> {
    let catalog: Catalog = serde_json::from_str(json)?;
    validate_catalog(&catalog)?;
    Ok(catalog)
}

fn validate_catalog(catalog: &Catalog) -> AppResult<()> {
    if catalog.catalog_version.trim().is_empty() {
        return Err(AppError::BadRequest(
            "catalogVersion must not be empty".to_string(),
        ));
    }
    if catalog.points.is_empty() {
        return Err(AppError::BadRequest(
            "catalog must contain at least one point".to_string(),
        ));
    }

    let mut seen = std::collections::HashSet::new();
    for p in &catalog.points {
        if !seen.insert(p.id.clone()) {
            return Err(AppError::BadRequest(format!(
                "duplicate point id: {}",
                p.id
            )));
        }
    }

    for c in &catalog.closures {
        if c.to <= c.from {
            return Err(AppError::BadRequest(format!(
                "closure {} has to <= from",
                c.event_id
            )));
        }
        if !catalog.points.iter().any(|p| p.id == c.point_id) {
            return Err(AppError::BadRequest(format!(
                "closure {} references unknown point {}",
                c.event_id, c.point_id
            )));
        }
    }

    Ok(())
}
