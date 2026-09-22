//! Background jobs that aren't part of the check pipeline: self-contained
//! tick functions the runtime schedules (daily scheduler, manual invocation
//! in tests), the shared `periodic` purge-loop runner, the per-monitor
//! silence sweep and the outbound dead-man ping.

pub mod custom_domains;
pub mod disposable_refresh;
pub mod heartbeat_nudge;
pub mod periodic;
pub mod purge_deleted;
pub mod retention;
pub mod silence;
pub mod snitch;
