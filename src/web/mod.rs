//! Server-rendered UI module.
//!
//! Thin presentation layer that consumes existing `/api/v1/*` JSON endpoints
//! and renders askama templates. It owns no domain logic and no mutation
//! routes — every UI mutation hits an existing API endpoint directly.

pub mod assets;
pub mod auth;
pub mod avatar;
pub mod client_ip;
pub mod deletion_receipt;
pub mod display_prefs;
pub mod error;
pub mod filters;
pub mod flash;
pub mod host;
pub mod login_hint;
pub mod robots;
pub mod routes;
pub mod theme;
pub mod time_format;
pub mod views;

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
pub use routes::routes;
