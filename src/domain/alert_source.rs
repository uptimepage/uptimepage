//! A tool that sends alerts in, and the route its pages take.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::WriteSource;
use super::alert::TargetAlerts;
use super::target::{default_renotify_interval_secs, default_true, double_option};
use super::user::UserId;

pub const MAX_ALERT_SOURCE_NAME_LEN: usize = 100;
/// A shorter shared secret is guessable offline from one signed request.
pub const MIN_SIGNING_SECRET_LEN: usize = 16;
pub const MAX_SIGNING_SECRET_LEN: usize = 256;

/// The tool's payload format. Fixed at create: a different tool is a
/// different source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlertSourceKind {
    Generic,
    Alertmanager,
    Grafana,
    Datadog,
}

impl AlertSourceKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Alertmanager => "alertmanager",
            Self::Grafana => "grafana",
            Self::Datadog => "datadog",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "alertmanager" => Self::Alertmanager,
            "grafana" => Self::Grafana,
            "datadog" => Self::Datadog,
            _ => Self::Generic,
        }
    }

    /// Only Grafana signs what it sends.
    pub fn signs(self) -> bool {
        self == Self::Grafana
    }
}

/// One incident per group the tool sends, or one per alert in it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AlertGrouping {
    #[default]
    Group,
    Alert,
}

impl AlertGrouping {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Alert => "alert",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "alert" => Self::Alert,
            _ => Self::Group,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlertSource {
    pub id: Uuid,
    pub name: String,
    pub kind: AlertSourceKind,
    /// Off while paused: firings open nothing, resolves still close.
    pub enabled: bool,
    pub plan_hold_at: Option<DateTime<Utc>>,
    /// `None` when the KEK rotated out and the sealed copy can't be opened.
    pub token: Option<String>,
    pub token_rotated_at: Option<DateTime<Utc>>,
    pub prev_token_expires_at: Option<DateTime<Utc>>,
    pub prev_token_last_used_at: Option<DateTime<Utc>>,
    /// A signing secret is set, so unsigned requests are refused.
    pub signed: bool,
    pub grouping: AlertGrouping,
    pub escalation_policy_id: Option<Uuid>,
    pub alerts: TargetAlerts,
    pub notify_recovery: bool,
    pub renotify_interval_secs: u32,
    pub last_event_at: Option<DateTime<Utc>>,
    /// No live policy of its own, no live org default and no enabled bound
    /// channel. Refused on write, but a policy or channel change elsewhere can
    /// leave a source here.
    pub routes_to_nobody: bool,
    pub write_source: WriteSource,
    pub created_at: DateTime<Utc>,
    pub created_by: Option<UserId>,
    pub updated_at: DateTime<Utc>,
    pub updated_by: Option<UserId>,
}

impl AlertSource {
    /// The superseded token's expiry while it is still accepted.
    pub fn open_overlap(&self) -> Option<DateTime<Utc>> {
        self.prev_token_expires_at.filter(|t| *t > Utc::now())
    }
}

/// Where a tool posts when it sends the source's token as a bearer.
pub fn hook_url(public_base_url: &str) -> String {
    format!("{}/hooks/alerts", public_base_url.trim_end_matches('/'))
}

/// The URL a source's tool posts to, for a tool that sets no header.
pub fn ingest_url(public_base_url: &str, token: &str) -> String {
    format!("{}/{token}", hook_url(public_base_url))
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NewAlertSource {
    #[schema(max_length = 100)]
    pub name: String,
    pub kind: AlertSourceKind,
    #[serde(default)]
    pub grouping: AlertGrouping,
    #[serde(default)]
    #[schema(nullable = true)]
    pub escalation_policy_id: Option<Uuid>,
    #[serde(default, deserialize_with = "TargetAlerts::deserialize_strict")]
    #[schema(value_type = Vec<crate::domain::alert::AlertBindingInput>)]
    pub alerts: TargetAlerts,
    #[serde(default = "default_true")]
    pub notify_recovery: bool,
    #[serde(default = "default_renotify_interval_secs")]
    pub renotify_interval_secs: u32,
    /// Grafana only: the HMAC secret its contact point signs with.
    #[serde(default)]
    #[schema(nullable = true, min_length = 16, max_length = 256)]
    pub signing_secret: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AlertSourceUpdate {
    #[schema(max_length = 100)]
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub grouping: Option<AlertGrouping>,
    #[serde(default, deserialize_with = "double_option")]
    #[schema(nullable = true, value_type = Option<Uuid>)]
    pub escalation_policy_id: Option<Option<Uuid>>,
    #[serde(default, deserialize_with = "TargetAlerts::deserialize_strict_opt")]
    #[schema(value_type = Option<Vec<crate::domain::alert::AlertBindingInput>>)]
    pub alerts: Option<TargetAlerts>,
    pub notify_recovery: Option<bool>,
    pub renotify_interval_secs: Option<u32>,
    /// Null removes the secret and stops checking signatures.
    #[serde(default, deserialize_with = "double_option")]
    #[schema(nullable = true, value_type = Option<String>, min_length = 16, max_length = 256)]
    pub signing_secret: Option<Option<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_groupings_round_trip_through_the_db() {
        for kind in [
            AlertSourceKind::Generic,
            AlertSourceKind::Alertmanager,
            AlertSourceKind::Grafana,
            AlertSourceKind::Datadog,
        ] {
            assert_eq!(AlertSourceKind::from_db(kind.as_db_str()), kind);
        }
        for grouping in [AlertGrouping::Group, AlertGrouping::Alert] {
            assert_eq!(AlertGrouping::from_db(grouping.as_db_str()), grouping);
        }
    }

    #[test]
    fn a_patch_tells_a_cleared_field_from_an_absent_one() {
        let cleared: AlertSourceUpdate =
            serde_json::from_str(r#"{"escalation_policy_id": null, "signing_secret": null}"#)
                .unwrap();
        assert_eq!(cleared.escalation_policy_id, Some(None));
        assert_eq!(cleared.signing_secret, Some(None));
        let absent: AlertSourceUpdate = serde_json::from_str("{}").unwrap();
        assert_eq!(absent.escalation_policy_id, None);
        assert_eq!(absent.signing_secret, None);
    }

    #[test]
    fn kind_cannot_be_patched() {
        assert!(serde_json::from_str::<AlertSourceUpdate>(r#"{"kind": "grafana"}"#).is_err());
    }
}
