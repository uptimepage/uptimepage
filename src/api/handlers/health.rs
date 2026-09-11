use crate::api::json::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;
use utoipa::ToSchema;

use crate::app::AppState;

#[derive(Debug, Serialize, ToSchema)]
pub struct HealthResponse {
    #[schema(example = "ok")]
    pub status: &'static str,
}

/// Per-dependency readiness, so a 503 names which store is down instead of a
/// bare "not_ready".
#[derive(Debug, Serialize, ToSchema)]
pub struct ReadinessResponse {
    #[schema(example = "ready")]
    pub status: &'static str,
    pub postgres: DependencyState,
    pub clickhouse: DependencyState,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DependencyState {
    Up,
    Down,
}

impl From<bool> for DependencyState {
    fn from(ok: bool) -> Self {
        if ok { Self::Up } else { Self::Down }
    }
}

/// Probe endpoints that every health-conscious caller — Caddy active
/// health check, Docker healthcheck, k8s probes, the trace-span skip,
/// the access-log skip, the host-dispatch bypass — needs to recognise.
/// One source of truth so a new probe path can never go silently
/// undetected by one of the call sites (the 503 outage on 2026-05-21
/// happened because the marketing dispatcher was missing this check).
pub const HEALTH_PATHS: &[&str] = &["/healthz", "/readyz"];

/// True when `path` is one of [`HEALTH_PATHS`]. Cheap `contains` over a
/// 2-element slice — fine on the per-request hot path.
pub fn is_health_path(path: &str) -> bool {
    HEALTH_PATHS.contains(&path)
}

#[utoipa::path(
    get,
    path = "/healthz",
    tag = "health",
    summary = "Liveness probe",
    description = "Always returns 200 if the process is alive. Does NOT check dependencies.",
    responses(
        (status = 200, description = "Service is running", body = HealthResponse,
            example = json!({"status": "ok"})),
    ),
)]
pub async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[utoipa::path(
    get,
    path = "/readyz",
    tag = "health",
    summary = "Readiness probe",
    description = "Returns 200 only if all critical dependencies (Postgres, ClickHouse) are reachable. The body reports each dependency individually.",
    responses(
        (status = 200, description = "Ready to serve traffic", body = ReadinessResponse,
            example = json!({"status": "ready", "postgres": "up", "clickhouse": "up"})),
        (status = 503, description = "A dependency is unavailable", body = ReadinessResponse,
            example = json!({"status": "not_ready", "postgres": "up", "clickhouse": "down"})),
    ),
)]
pub async fn readyz(State(state): State<AppState>) -> (StatusCode, Json<ReadinessResponse>) {
    let ready = crate::app::probe_readiness(&state.target_store, &state.results_store).await;
    let body = ReadinessResponse {
        status: if ready.all_ok() { "ready" } else { "not_ready" },
        postgres: ready.postgres.into(),
        clickhouse: ready.clickhouse.into(),
    };
    let code = if ready.all_ok() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_paths_match_route_mounts() {
        // Lock the contract: the two probe endpoints recognised by the
        // dispatch bypass, the access-log skip, and the trace-span skip
        // are exactly the routes mounted in `api::routes`. A new probe
        // route MUST be added here in lockstep — otherwise it would 503
        // through Caddy's active health check the moment marketing is
        // enabled (the bug from 2026-05-21).
        assert!(is_health_path("/healthz"));
        assert!(is_health_path("/readyz"));
        assert!(!is_health_path("/health"));
        assert!(!is_health_path("/healthz/extra"));
        assert!(!is_health_path("/"));
        assert!(!is_health_path(""));
    }

    #[test]
    fn dependency_state_serializes_lowercase() {
        assert!(matches!(DependencyState::from(true), DependencyState::Up));
        assert!(matches!(
            DependencyState::from(false),
            DependencyState::Down
        ));
        assert_eq!(
            serde_json::to_string(&DependencyState::from(false)).unwrap(),
            "\"down\""
        );
    }
}
