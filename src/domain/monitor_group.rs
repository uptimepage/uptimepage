use chrono::{DateTime, Utc};
use serde::Deserialize;
use utoipa::ToSchema;
use uuid::Uuid;

use super::WriteSource;
use super::alert::TargetAlerts;
use super::target::{Target, default_renotify_interval_secs, default_true};
use super::user::UserId;

pub const MAX_GROUP_NAME_LEN: usize = 50;

/// A group name as every write stores it: `str::trim`, the policy the
/// database's `group_name_trim` mirrors, and blank meaning no group.
pub fn normalize_group_name(name: &str) -> Option<&str> {
    Some(name.trim()).filter(|n| !n.is_empty())
}

/// Members named in a sentence: "NS", "NS and A", "NS, A and MX", or the
/// first two and a count past three.
pub fn member_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [only] => only.clone(),
        [a, b] => format!("{a} and {b}"),
        [a, b, c] => format!("{a}, {b} and {c}"),
        [a, b, rest @ ..] => format!("{a}, {b} and {} more", rest.len()),
    }
}

/// Closing note of a group incident whose group was deleted, naming the
/// members it hands back to their own channels.
pub fn group_deleted_message(still_down: &[String]) -> String {
    match still_down {
        [] => super::incident::GROUP_DELETED_MESSAGE.into(),
        [one] => format!("Group deleted while {one} is still down; it now alerts on its own."),
        many => format!(
            "Group deleted while {} are still down; they now alert on their own.",
            member_list(many)
        ),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MonitorGroup {
    pub id: Uuid,
    pub name: String,
    pub alert_as_one: bool,
    pub alerts: TargetAlerts,
    pub renotify_interval_secs: u32,
    pub alert_again: bool,
    pub tags: Vec<String>,
    pub escalation_policy_id: Option<Uuid>,
    pub write_source: WriteSource,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<UserId>,
    pub updated_at: DateTime<Utc>,
    pub updated_by: Option<UserId>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewMonitorGroup {
    #[schema(max_length = 50)]
    pub name: String,
    #[serde(default)]
    pub alert_as_one: bool,
    #[serde(default, deserialize_with = "TargetAlerts::deserialize_strict")]
    #[schema(value_type = Vec<crate::domain::alert::AlertBindingInput>)]
    pub alerts: TargetAlerts,
    #[serde(default = "default_renotify_interval_secs")]
    pub renotify_interval_secs: u32,
    #[serde(default = "default_true")]
    pub alert_again: bool,
    #[serde(default)]
    #[schema(max_items = 50)]
    pub tags: Vec<String>,
}

impl NewMonitorGroup {
    /// A label group, as a monitor's `group_name` creates one.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            alert_as_one: false,
            alerts: TargetAlerts::default(),
            renotify_interval_secs: default_renotify_interval_secs(),
            alert_again: true,
            tags: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MonitorGroupUpdate {
    #[schema(max_length = 50)]
    pub name: Option<String>,
    pub alert_as_one: Option<bool>,
    #[serde(default, deserialize_with = "TargetAlerts::deserialize_strict_opt")]
    #[schema(value_type = Option<Vec<crate::domain::alert::AlertBindingInput>>)]
    pub alerts: Option<TargetAlerts>,
    pub renotify_interval_secs: Option<u32>,
    pub alert_again: Option<bool>,
    #[schema(max_items = 50)]
    pub tags: Option<Vec<String>>,
}

/// What a page is about: one monitor, or a group that alerts as one.
#[derive(Debug, Clone, Copy)]
pub enum Subject<'a> {
    Monitor(&'a Target),
    Group(&'a MonitorGroup),
}

impl Subject<'_> {
    pub fn name(&self) -> &str {
        match self {
            Self::Monitor(t) => &t.name,
            Self::Group(g) => &g.name,
        }
    }

    pub fn alerts(&self) -> &TargetAlerts {
        match self {
            Self::Monitor(t) => &t.alerts,
            Self::Group(g) => &g.alerts,
        }
    }

    pub fn tags(&self) -> &[String] {
        match self {
            Self::Monitor(t) => &t.tags,
            Self::Group(g) => &g.tags,
        }
    }

    pub fn renotify_interval_secs(&self) -> u32 {
        match self {
            Self::Monitor(t) => t.renotify_interval_secs,
            Self::Group(g) => g.renotify_interval_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_trimmed_and_blank_is_no_group() {
        assert_eq!(normalize_group_name("  DNS\u{2003}"), Some("DNS"));
        assert_eq!(normalize_group_name("\u{00A0}\u{3000} "), None);
        assert_eq!(normalize_group_name(""), None);
        assert_eq!(normalize_group_name("\u{200B}"), Some("\u{200B}"));
    }

    #[test]
    fn members_read_as_a_sentence() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(member_list(&names(&["NS"])), "NS");
        assert_eq!(member_list(&names(&["NS", "A"])), "NS and A");
        assert_eq!(member_list(&names(&["NS", "A", "MX"])), "NS, A and MX");
        assert_eq!(
            member_list(&names(&["NS", "A", "MX", "TXT"])),
            "NS, A and 2 more"
        );
        assert_eq!(
            group_deleted_message(&names(&["NS", "A"])),
            "Group deleted while NS and A are still down; they now alert on their own."
        );
        assert_eq!(
            group_deleted_message(&names(&["NS"])),
            "Group deleted while NS is still down; it now alerts on its own."
        );
    }
}
