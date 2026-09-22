//! What every HTTP surface reads off a request before its own work starts:
//! the caller (session, API token, agent, operator), the client IP, the host
//! a status page is served on, the cookies that carry state between pages
//! (flash, login hint, display preferences, deletion receipt), and the two
//! layers applied around every route: per-route metrics and the per-subject
//! rate limit.

pub mod auth;
pub mod client_ip;
pub mod custom_domains;
pub mod deletion_receipt;
pub mod display_prefs;
pub mod flash;
pub mod host;
pub mod http_metrics;
pub mod login_hint;
pub mod rate_limit;
pub mod theme;
pub mod time_format;

pub use auth::agent::AgentIdentity;
pub use auth::api_token::{BrowserUser, VerifiedBrowserUser};
pub use auth::authz::{
    Authorized, ChannelsDelete, ChannelsExecute, ChannelsRead, ChannelsWrite, IncidentsRead,
    IncidentsWrite, MaintenanceDelete, MaintenanceRead, MaintenanceWrite, OnCallRead, OnCallWrite,
    OwnerAuthorized, RequestSource, StatusPageDelete, StatusPageRead, StatusPageWrite,
    TargetsDelete, TargetsExecute, TargetsRead, TargetsWrite, TokenScopes, VariablesRead,
    VariablesWrite,
};
pub use auth::operator::OperatorAuth;
pub use auth::{AuthedBrowser, CurrentOrg, CurrentUser, PendingDeletionUser, Session, User};
pub use host::{ResolvedStatusPage, StatusPageHost, extract_status_slug};

/// Probe endpoints that every health-conscious caller — Caddy active
/// health check, Docker healthcheck, k8s probes, the trace-span skip,
/// the access-log skip, the host-dispatch bypass — needs to recognise.
/// One source of truth so a new probe path can never go silently
/// undetected by one of the call sites (the 503 outage on 2026-05-21
/// happened because the marketing dispatcher was missing this check).
pub const HEALTH_PATHS: &[&str] = &["/healthz", "/readyz"];

/// Where the MCP transport listens; the MCP host serves this and discovery only.
pub const MCP_PATH: &str = "/mcp";

/// True when `path` is one of [`HEALTH_PATHS`]. Cheap `contains` over a
/// 2-element slice — fine on the per-request hot path.
pub fn is_health_path(path: &str) -> bool {
    HEALTH_PATHS.contains(&path)
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
}
