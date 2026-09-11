use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::WriteSource;
use super::alert::TargetAlerts;
use super::check::CheckSpec;
use super::result::CheckStatus;

/// Tag bounds, applied by every write path.
pub const MAX_TAGS_PER_TARGET: usize = 50;
pub const MAX_TAG_LEN: usize = 50;

/// How many regions must agree a monitor is down before it alerts. `Any`,
/// `Majority`, and `All` track the live region count; `Count` is a fixed number
/// the user chose. Resolved to a concrete threshold by [`Self::required`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum RegionIncidentPolicy {
    /// One region down is enough.
    Any,
    /// More than half the regions (`n/2 + 1`).
    #[default]
    Majority,
    /// Every region.
    All,
    /// A fixed number of regions.
    Count(u32),
}

impl RegionIncidentPolicy {
    /// The concrete number of down regions needed, given how many are in play.
    /// Always clamped to `1..=region_count` so it can never be impossible.
    pub fn required(&self, region_count: usize) -> usize {
        let n = match self {
            Self::Any => 1,
            Self::Majority => region_count / 2 + 1,
            Self::All => region_count,
            Self::Count(c) => *c as usize,
        };
        n.clamp(1, region_count.max(1))
    }

    /// Below quorum yields `Degraded`: the incident writer refused to open an
    /// outage here. Reads a latest status, not a run, so no `alert_confirmations`.
    pub fn fold_regions(
        &self,
        latest_per_region: impl IntoIterator<Item = CheckStatus>,
    ) -> Option<CheckStatus> {
        let mut reported = 0usize;
        let mut bad = 0usize;
        let mut worst = CheckStatus::Up;
        for s in latest_per_region {
            reported += 1;
            if s.is_bad() {
                bad += 1;
                if s.severity_rank() > worst.severity_rank() {
                    worst = s;
                }
            }
        }
        if reported == 0 {
            return None;
        }
        if bad == 0 {
            return Some(CheckStatus::Up);
        }
        if bad < self.required(reported) {
            return Some(CheckStatus::Degraded);
        }
        Some(worst)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Target {
    pub id: Uuid,
    pub name: String,
    pub check: CheckSpec,
    /// Check interval in seconds.
    #[serde(with = "duration_secs")]
    #[schema(value_type = u64, example = 60, minimum = 10)]
    pub interval: Duration,
    pub enabled: bool,
    pub tags: Vec<String>,
    #[serde(default)]
    pub alerts: TargetAlerts,
    /// Consecutive failing checks before this monitor alerts. Min 1.
    #[serde(default = "default_alert_confirmations")]
    pub alert_confirmations: u32,
    /// Whether a recovery is announced to the monitor's channels.
    #[serde(default = "default_true")]
    pub notify_recovery: bool,
    /// Seconds before the first reminder while an outage stays unacknowledged;
    /// each further reminder waits twice as long, up to a day. 0 = off.
    #[serde(default = "default_renotify_interval_secs")]
    pub renotify_interval_secs: u32,
    /// How multi-region health folds into incidents for this monitor.
    #[serde(default)]
    pub region_policy: RegionIncidentPolicy,
    /// Operator-side grouping (independent of any status page's grouping).
    #[serde(default)]
    #[schema(example = "API & Web", nullable = true, max_length = 50)]
    pub group_name: Option<String>,
    /// FK to `users.id`. Nullable; cleared if the user is removed.
    #[serde(default)]
    #[schema(nullable = true)]
    pub owner_user_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Where this target was last changed from (UI, API, or Terraform).
    #[serde(default)]
    pub write_source: WriteSource,
    /// Set while the plan no longer covers this monitor. The row is kept whole
    /// and stops being probed; the plan growing back releases it untouched.
    /// Independent of `enabled`, which stays the customer's own switch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = true)]
    pub plan_hold_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewTarget {
    pub name: String,
    pub check: CheckSpec,
    /// Check interval in seconds.
    #[serde(with = "duration_secs")]
    #[schema(value_type = u64, example = 60, minimum = 10)]
    pub interval: Duration,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    #[schema(max_items = 50)]
    pub tags: Vec<String>,
    #[serde(default, deserialize_with = "TargetAlerts::deserialize_strict")]
    #[schema(value_type = Vec<crate::domain::alert::AlertBindingInput>)]
    pub alerts: TargetAlerts,
    /// Consecutive failing checks before this monitor alerts. Min 1.
    #[serde(default = "default_alert_confirmations")]
    pub alert_confirmations: u32,
    #[serde(default = "default_true")]
    pub notify_recovery: bool,
    /// Seconds before the first reminder while an outage stays unacknowledged;
    /// each further reminder waits twice as long, up to a day. 0 = off.
    #[serde(default = "default_renotify_interval_secs")]
    pub renotify_interval_secs: u32,
    /// Detection policy. Omit to take the derived default — quorum-majority when
    /// the monitor lands in more than one region, any-down for a single region.
    #[serde(default)]
    pub region_policy: Option<RegionIncidentPolicy>,
    #[serde(default)]
    #[schema(nullable = true, max_length = 50)]
    pub group_name: Option<String>,
    #[serde(default)]
    #[schema(nullable = true)]
    pub owner_user_id: Option<Uuid>,
    /// Probe regions to assign at creation. Omit for the default set; refused on a heartbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = true)]
    pub regions: Option<Vec<String>>,
}

/// A monitor with the regions it probes from, resolved before the write so
/// both land in one transaction. Empty for a heartbeat, which is pinged.
#[derive(Debug, Clone)]
pub struct NewTargetWithRegions {
    pub target: NewTarget,
    pub regions: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetUpdate {
    pub name: Option<String>,
    pub check: Option<CheckSpec>,
    /// Check interval in seconds.
    #[serde(default, with = "duration_secs_opt")]
    #[schema(value_type = Option<u64>)]
    pub interval: Option<Duration>,
    pub enabled: Option<bool>,
    #[serde(default)]
    pub region_policy: Option<RegionIncidentPolicy>,
    pub alert_confirmations: Option<u32>,
    pub notify_recovery: Option<bool>,
    pub renotify_interval_secs: Option<u32>,
    #[schema(max_items = 50)]
    pub tags: Option<Vec<String>>,
    #[serde(default, deserialize_with = "TargetAlerts::deserialize_strict_opt")]
    #[schema(value_type = Option<Vec<crate::domain::alert::AlertBindingInput>>)]
    pub alerts: Option<TargetAlerts>,
    #[serde(default, deserialize_with = "double_option")]
    #[schema(nullable = true, value_type = Option<String>)]
    pub group_name: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option")]
    #[schema(nullable = true, value_type = Option<Uuid>)]
    pub owner_user_id: Option<Option<Uuid>>,
}

/// Lifts the inner `Option<T>` into `Some(Option<T>)` so a missing field
/// stays `None` (via `#[serde(default)]` = "leave unchanged") while an
/// explicit JSON `null` becomes `Some(None)` (the "clear" intent).
pub(crate) fn double_option<'de, T, D>(d: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

fn default_true() -> bool {
    true
}

fn default_alert_confirmations() -> u32 {
    2
}

fn default_renotify_interval_secs() -> u32 {
    3600
}

mod duration_secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_secs())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let secs = u64::deserialize(d)?;
        Ok(Duration::from_secs(secs))
    }
}

mod duration_secs_opt {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(v) => s.serialize_some(&v.as_secs()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        let secs = Option::<u64>::deserialize(d)?;
        Ok(secs.map(Duration::from_secs))
    }
}

#[cfg(test)]
mod region_fold_tests {
    use super::*;

    const P: RegionIncidentPolicy = RegionIncidentPolicy::Majority;

    fn fold(policy: RegionIncidentPolicy, s: &[CheckStatus]) -> Option<CheckStatus> {
        policy.fold_regions(s.iter().copied())
    }

    #[test]
    fn nothing_reported_has_no_status() {
        assert_eq!(fold(P, &[]), None);
    }

    #[test]
    fn all_up_is_up() {
        use CheckStatus::*;
        assert_eq!(fold(P, &[Up, Up, Up]), Some(Up));
    }

    #[test]
    fn one_failing_region_of_three_is_degraded_not_down() {
        use CheckStatus::*;
        assert_eq!(fold(P, &[Up, Up, Down]), Some(Degraded));
        assert_eq!(fold(P, &[Up, Up, Error]), Some(Degraded));
    }

    #[test]
    fn quorum_reached_reports_the_worst_failure() {
        use CheckStatus::*;
        assert_eq!(fold(P, &[Up, Error, Down]), Some(Down));
        assert_eq!(fold(P, &[Degraded, Down, Up]), Some(Down));
        assert_eq!(fold(P, &[Degraded, Error, Up]), Some(Error));
    }

    #[test]
    fn single_region_keeps_its_own_verdict() {
        use CheckStatus::*;
        assert_eq!(fold(P, &[Down]), Some(Down));
        assert_eq!(fold(P, &[Error]), Some(Error));
        assert_eq!(fold(P, &[Up]), Some(Up));
    }

    #[test]
    fn any_policy_reports_a_lone_failure() {
        use CheckStatus::*;
        assert_eq!(fold(RegionIncidentPolicy::Any, &[Up, Up, Down]), Some(Down));
    }

    #[test]
    fn all_policy_softens_until_every_region_agrees() {
        use CheckStatus::*;
        let all = RegionIncidentPolicy::All;
        assert_eq!(fold(all, &[Down, Down, Up]), Some(Degraded));
        assert_eq!(fold(all, &[Down, Down, Down]), Some(Down));
    }

    #[test]
    fn count_policy_uses_its_own_threshold() {
        use CheckStatus::*;
        let two = RegionIncidentPolicy::Count(2);
        assert_eq!(fold(two, &[Down, Up, Up]), Some(Degraded));
        assert_eq!(fold(two, &[Down, Down, Up]), Some(Down));
    }

    #[test]
    fn degraded_regions_count_toward_quorum() {
        use CheckStatus::*;
        assert_eq!(fold(P, &[Degraded, Degraded, Up]), Some(Degraded));
    }

    #[test]
    fn quorum_denominator_is_what_reported() {
        use CheckStatus::*;
        assert_eq!(fold(P, &[Up, Down]), Some(Degraded));
    }
}
