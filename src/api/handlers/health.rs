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
    let ready =
        crate::observability::readiness::probe_readiness(&state.target_store, &state.results_store)
            .await;
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
