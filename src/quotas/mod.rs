//! Resource quotas + request rate limiting.
//!
//! - [`service::QuotaService`] resolves an org's account and that account's
//!   effective plan (cached) and provides the friendly-error quota checks;
//!   every count spans the account's live orgs, and the race-safe guarantee is
//!   in the store INSERTs that pool the same way and take the same limit.
//! - [`effective`] clamps what the scheduler and the agents are handed to the
//!   plan's ceilings.
//! - [`holds`] parks what a shrunken plan no longer covers, without deleting
//!   any of it.
//! - [`overrides`] is the operator's write path for a per-account cap change.
//! - [`ratelimit::RateLimitService`] is the per-account / per-user limiter.
//! - [`middleware::rate_limit_middleware`] wires the limiter into `/api/v1`.

pub mod effective;
pub mod holds;
pub mod middleware;
pub mod overrides;
pub mod ratelimit;
pub mod service;

pub use effective::{PlanGoverned, governed_interval};
pub use holds::{PlanSource, Reconciled, reconcile_account, reconcile_after_change};
pub use middleware::rate_limit_middleware;
pub use ratelimit::{RateLimitCategory, RateLimitKey, RateLimitService};
pub use service::QuotaService;
