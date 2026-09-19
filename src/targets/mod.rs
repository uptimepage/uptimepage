//! Monitor read models and defaults shared by every surface that shows or
//! creates one, so the JSON API and the HTML forms cannot answer differently.

pub mod heartbeat;
pub mod regions;

pub use heartbeat::{
    CadenceAdviceView, HeartbeatInfo, heartbeat_info, heartbeat_info_from, observed_cadence,
};
pub use regions::{default_region_set, flow_capable_set};
