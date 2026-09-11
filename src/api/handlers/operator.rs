//! Operator (instance-admin) HTTP surface: regions + agents. Every handler is
//! gated by [`OperatorAuth`] (a static bearer secret), cross-tenant, and kept
//! off the tenant `/api/v1` surface. Agent creation mints the agent's token and
//! returns it exactly once.

use crate::api::json::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::error::codes;
use crate::app::AppState;
use crate::error::{AppError, Result};
use crate::storage::operator::{DeleteRegion, OperatorRepo};
use crate::web::OperatorAuth;

const MAX_NAME: usize = 80;
const MAX_REGION_ID: usize = 40;
const MAX_CITY: usize = 120;

fn repo(state: &AppState) -> Result<OperatorRepo> {
    Ok(OperatorRepo::new(state.require_db()?.clone()))
}

fn validate_name(name: &str) -> Result<&str> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_NAME {
        return Err(AppError::bad_request(
            codes::INVALID_NAME,
            format!("name must be 1..={MAX_NAME} characters"),
        ));
    }
    Ok(trimmed)
}

/// Region ids are slugs: lowercase alphanumerics and hyphens, no leading or
/// trailing hyphen. They surface in URLs and ClickHouse `region` values, so
/// keep them tidy and stable.
fn validate_region_id(id: &str) -> Result<&str> {
    let id = id.trim();
    let ok = !id.is_empty()
        && id.len() <= MAX_REGION_ID
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.starts_with('-')
        && !id.ends_with('-');
    if !ok {
        return Err(AppError::bad_request(
            codes::REGION_INVALID,
            "region id must be a lowercase slug (a-z, 0-9, -)",
        ));
    }
    Ok(id)
}

/// Optional continent slug — must be one of `domain::region::Continent`.
fn validate_continent(continent: Option<&str>) -> Result<Option<String>> {
    use crate::domain::region::Continent;
    match continent.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(c) if Continent::parse(c).is_some() => Ok(Some(c.to_string())),
        Some(_) => Err(AppError::bad_request(
            codes::REGION_INVALID,
            format!("continent must be one of: {}", Continent::slug_list()),
        )),
    }
}

/// Optional ISO 3166-1 alpha-2 country, normalized to uppercase.
fn validate_country_code(code: Option<&str>) -> Result<Option<String>> {
    match code.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(c) => crate::domain::region::normalize_country_code(c)
            .map(Some)
            .ok_or_else(|| {
                AppError::bad_request(
                    codes::REGION_INVALID,
                    "country_code must be a 2-letter ISO 3166-1 alpha-2 code",
                )
            }),
    }
}

fn validate_city(city: Option<&str>) -> Result<Option<&str>> {
    if let Some(c) = city
        && c.len() > MAX_CITY
    {
        return Err(AppError::bad_request(
            codes::REGION_INVALID,
            format!("city must be at most {MAX_CITY} characters"),
        ));
    }
    Ok(city)
}

/// Coordinates are optional but all-or-nothing, and within valid ranges.
fn validate_coords(lat: Option<f64>, lon: Option<f64>) -> Result<(Option<f64>, Option<f64>)> {
    match (lat, lon) {
        (None, None) => Ok((None, None)),
        (Some(la), Some(lo)) if (-90.0..=90.0).contains(&la) && (-180.0..=180.0).contains(&lo) => {
            Ok((Some(la), Some(lo)))
        }
        (Some(_), Some(_)) => Err(AppError::bad_request(
            codes::REGION_INVALID,
            "latitude must be -90..=90 and longitude -180..=180",
        )),
        _ => Err(AppError::bad_request(
            codes::REGION_INVALID,
            "latitude and longitude must be set together",
        )),
    }
}

// ── regions ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct RegionView {
    pub id: String,
    pub name: String,
    pub city: Option<String>,
    pub country_code: Option<String>,
    pub continent: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub enabled: bool,
    pub default_selected: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewRegion {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub continent: Option<String>,
    #[serde(default)]
    pub latitude: Option<f64>,
    #[serde(default)]
    pub longitude: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRegion {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub country_code: Option<String>,
    #[serde(default)]
    pub continent: Option<String>,
    #[serde(default)]
    pub latitude: Option<f64>,
    #[serde(default)]
    pub longitude: Option<f64>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub default_selected: Option<bool>,
}

pub async fn list_regions(
    _: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Vec<RegionView>>> {
    let regions = repo(&state)?.list_regions().await?;
    Ok(Json(
        regions
            .into_iter()
            .map(|r| RegionView {
                id: r.id,
                name: r.name,
                city: r.city,
                country_code: r.country_code,
                continent: r.continent,
                latitude: r.latitude,
                longitude: r.longitude,
                enabled: r.enabled,
                default_selected: r.default_selected,
                created_at: r.created_at,
            })
            .collect(),
    ))
}

pub async fn create_region(
    _: OperatorAuth,
    State(state): State<AppState>,
    Json(req): Json<NewRegion>,
) -> Result<StatusCode> {
    let id = validate_region_id(&req.id)?;
    let name = validate_name(&req.name)?;
    let city = validate_city(req.city.as_deref())?;
    let country = validate_country_code(req.country_code.as_deref())?;
    let continent = validate_continent(req.continent.as_deref())?;
    let (lat, lon) = validate_coords(req.latitude, req.longitude)?;
    if repo(&state)?
        .create_region(
            id,
            name,
            city,
            country.as_deref(),
            continent.as_deref(),
            lat,
            lon,
        )
        .await?
    {
        Ok(StatusCode::CREATED)
    } else {
        Err(AppError::conflict(
            codes::REGION_EXISTS,
            "a region with that id already exists",
        ))
    }
}

pub async fn update_region(
    _: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateRegion>,
) -> Result<StatusCode> {
    if req.name.is_none()
        && req.city.is_none()
        && req.country_code.is_none()
        && req.continent.is_none()
        && req.latitude.is_none()
        && req.longitude.is_none()
        && req.enabled.is_none()
        && req.default_selected.is_none()
    {
        return Err(AppError::bad_request(
            codes::EMPTY_PATCH,
            "set at least one of name, city, country_code, continent, latitude, longitude, enabled",
        ));
    }
    let r = repo(&state)?;
    let mut found = false;
    if req.name.is_some()
        || req.city.is_some()
        || req.country_code.is_some()
        || req.continent.is_some()
        || req.latitude.is_some()
        || req.longitude.is_some()
    {
        let name = req.name.as_deref().map(validate_name).transpose()?;
        let city = validate_city(req.city.as_deref())?;
        let country = validate_country_code(req.country_code.as_deref())?;
        let continent = validate_continent(req.continent.as_deref())?;
        let (lat, lon) = validate_coords(req.latitude, req.longitude)?;
        found |= r
            .update_region(
                &id,
                name,
                city,
                country.as_deref(),
                continent.as_deref(),
                lat,
                lon,
            )
            .await?;
    }
    if let Some(enabled) = req.enabled {
        found |= r.set_region_enabled(&id, enabled).await?;
    }
    if let Some(on) = req.default_selected {
        found |= r.set_region_default_selected(&id, on).await?;
    }
    if found {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::not_found(
            codes::REGION_NOT_FOUND,
            "region not found",
        ))
    }
}

pub async fn delete_region(
    _: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode> {
    match repo(&state)?.delete_region(&id).await? {
        DeleteRegion::Deleted => Ok(StatusCode::NO_CONTENT),
        DeleteRegion::NotFound => Err(AppError::not_found(
            codes::REGION_NOT_FOUND,
            "region not found",
        )),
        DeleteRegion::InUse => Err(AppError::conflict(
            codes::REGION_IN_USE,
            "region still has agents or assigned targets",
        )),
    }
}

// ── agents ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct AgentView {
    pub id: Uuid,
    pub region: String,
    pub name: String,
    pub enabled: bool,
    pub token_prefix: String,
    pub last_seen_at: Option<DateTime<Utc>>,
    /// Enabled agent with no check-in within the staleness window (dead-man's
    /// switch). A disabled agent is never stale (it is intentionally dark).
    pub stale: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewAgent {
    pub region: String,
    pub name: String,
}

#[derive(Serialize)]
pub struct NewAgentResponse {
    pub id: Uuid,
    pub region: String,
    pub name: String,
    /// Raw token — shown exactly once; store it on the agent box now.
    pub token: String,
    pub token_prefix: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateAgent {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

pub async fn list_agents(
    _: OperatorAuth,
    State(state): State<AppState>,
) -> Result<Json<Vec<AgentView>>> {
    let agents = repo(&state)?.list_agents().await?;
    let threshold = state.cfg.operator.agent_stale_after_secs;
    let now = Utc::now();
    Ok(Json(
        agents
            .into_iter()
            .map(|a| {
                // Same rule as the background sweep: never-seen agents age from
                // creation, so a fresh agent inside the window isn't "stale".
                let since = a.last_seen_at.unwrap_or(a.created_at);
                let stale = a.enabled && (now - since).num_seconds().max(0) as u64 > threshold;
                AgentView {
                    id: a.id,
                    region: a.region,
                    name: a.name,
                    enabled: a.enabled,
                    token_prefix: a.token_prefix,
                    last_seen_at: a.last_seen_at,
                    stale,
                    created_at: a.created_at,
                }
            })
            .collect(),
    ))
}

pub async fn create_agent(
    _: OperatorAuth,
    State(state): State<AppState>,
    Json(req): Json<NewAgent>,
) -> Result<(StatusCode, Json<NewAgentResponse>)> {
    let region = validate_region_id(&req.region)?;
    let name = validate_name(&req.name)?;
    match repo(&state)?.create_agent(region, name).await? {
        Some((row, token)) => Ok((
            StatusCode::CREATED,
            Json(NewAgentResponse {
                id: row.id,
                region: row.region,
                name: row.name,
                token,
                token_prefix: row.token_prefix,
            }),
        )),
        None => Err(AppError::not_found(
            codes::REGION_NOT_FOUND,
            "region not found — create it first",
        )),
    }
}

pub async fn update_agent(
    _: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateAgent>,
) -> Result<StatusCode> {
    if req.name.is_none() && req.enabled.is_none() {
        return Err(AppError::bad_request(
            codes::EMPTY_PATCH,
            "set at least one of name, enabled",
        ));
    }
    let r = repo(&state)?;
    let mut found = false;
    if let Some(name) = req.name.as_deref() {
        let name = validate_name(name)?;
        found |= r.rename_agent(id, name).await?;
    }
    if let Some(enabled) = req.enabled {
        found |= r.set_agent_enabled(id, enabled).await?;
    }
    if found {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::not_found(
            codes::AGENT_NOT_FOUND,
            "agent not found",
        ))
    }
}

pub async fn delete_agent(
    _: OperatorAuth,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode> {
    if repo(&state)?.delete_agent(id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::not_found(
            codes::AGENT_NOT_FOUND,
            "agent not found",
        ))
    }
}
