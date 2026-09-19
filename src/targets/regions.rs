//! Which regions a monitor runs in when nobody chose: the plan-capped default
//! set, and the set that can run a flow at all.

use std::collections::HashSet;

use crate::app::AppState;
use crate::error::Result;

/// The default region set for a new monitor: the regions flagged
/// `default_selected`, capped at the plan's `max_regions` (default region kept
/// first when the cap bites), so the default can never exceed the quota. An
/// opted-out region stays pickable on the form, just unchecked.
pub fn default_region_set(
    preferred: Vec<String>,
    max_regions: i32,
    default_region: &str,
) -> Vec<String> {
    let cap = max_regions.max(1) as usize;
    let set: Vec<String> = if preferred.len() <= cap {
        preferred
    } else {
        // The control plane's own region leads when the cap bites, but only if
        // it is a default at all: an opted-out region must not seed itself back
        // in and displace one the operator actually chose.
        let mut v: Vec<String> = match preferred.iter().any(|r| r == default_region) {
            true => vec![default_region.to_string()],
            false => Vec::new(),
        };
        for r in preferred {
            if r != default_region && v.len() < cap {
                v.push(r);
            }
        }
        v
    };
    if set.is_empty() {
        vec![default_region.to_string()]
    } else {
        set
    }
}

/// Regions that can actually run a flow: agents self-reporting the capability,
/// plus the control-plane region when it runs the engine in-process.
pub async fn flow_capable_set(state: &AppState) -> Result<HashSet<String>> {
    let mut capable: HashSet<String> = state
        .target_store
        .flow_capable_regions()
        .await?
        .into_iter()
        .collect();
    if state.cfg.flow.enabled {
        capable.insert(state.cfg.scheduler.effective_default_region().to_string());
    }
    Ok(capable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_monitor_seeds_only_the_regions_flagged_as_defaults() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // Under no cap the flagged set seeds verbatim; an opted-out region is
        // absent from the input and so never appears.
        assert_eq!(
            default_region_set(
                ids(&["eu-frankfurt", "eu-helsinki", "us-east", "us-west"]),
                i32::MAX,
                "eu-helsinki",
            ),
            ids(&["eu-frankfurt", "eu-helsinki", "us-east", "us-west"])
        );

        // Under a plan cap the default region survives and the rest fill in order.
        assert_eq!(
            default_region_set(
                ids(&["eu-frankfurt", "eu-helsinki", "us-east", "us-west"]),
                3,
                "eu-helsinki",
            ),
            ids(&["eu-helsinki", "eu-frankfurt", "us-east"])
        );

        // Opting the control plane's own region out keeps it out even when the cap
        // bites — it must not displace a region the operator did choose.
        assert_eq!(
            default_region_set(
                ids(&["apac-sg", "eu-frankfurt", "us-east", "us-west"]),
                3,
                "eu-helsinki",
            ),
            ids(&["apac-sg", "eu-frankfurt", "us-east"])
        );

        // Every region opted out still leaves a monitor that is probed somewhere.
        assert_eq!(
            default_region_set(Vec::new(), 3, "eu-helsinki"),
            ids(&["eu-helsinki"])
        );
    }
}
