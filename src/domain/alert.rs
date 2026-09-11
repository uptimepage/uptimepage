//! Per-target alert bindings. A binding references an org-owned
//! [`crate::domain::NotificationChannel`] by id and carries only the
//! per-target firing policy — the transport and its secrets live in
//! `notification_channels`, not here.

use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct AlertBinding {
    /// Id of a channel in the caller's org (`notification_channels.id`). A
    /// binding is a pure delivery target; the firing policy (confirmations,
    /// recovery) lives on the monitor.
    #[schema(value_type = String, format = "uuid")]
    pub channel_id: Uuid,
}

/// A target's alert bindings. Serialised transparently as a JSON array so the
/// `targets.alerts` column is a list of `{channel_id}` objects.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(transparent)]
#[schema(value_type = Vec<AlertBinding>)]
pub struct TargetAlerts(pub Vec<AlertBinding>);

/// Request-side shape. The stored and agent-facing `AlertBinding` stays
/// lenient so a key added later never breaks an agent still on the old build.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingInput {
    channel_id: Uuid,
}

impl TargetAlerts {
    pub(crate) fn deserialize_strict<'de, D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Vec::<BindingInput>::deserialize(d).map(Self::from_inputs)
    }

    pub(crate) fn deserialize_strict_opt<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Self>, D::Error> {
        Option::<Vec<BindingInput>>::deserialize(d).map(|v| v.map(Self::from_inputs))
    }

    fn from_inputs(inputs: Vec<BindingInput>) -> Self {
        Self(
            inputs
                .into_iter()
                .map(|b| AlertBinding {
                    channel_id: b.channel_id,
                })
                .collect(),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &AlertBinding> {
        self.0.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_bare_binding() {
        let json = r#"[{"channel_id":"00000000-0000-0000-0000-000000000001"}]"#;
        let alerts: TargetAlerts = serde_json::from_str(json).unwrap();
        assert_eq!(alerts.iter().count(), 1);
    }

    #[test]
    fn deserialize_multiple_bindings() {
        let json = r#"[
            {"channel_id":"00000000-0000-0000-0000-000000000001"},
            {"channel_id":"00000000-0000-0000-0000-000000000002"}
        ]"#;
        let alerts: TargetAlerts = serde_json::from_str(json).unwrap();
        assert_eq!(alerts.iter().count(), 2);
    }

    #[test]
    fn empty_default_round_trips_as_array() {
        let alerts = TargetAlerts::default();
        assert!(alerts.is_empty());
        assert_eq!(serde_json::to_string(&alerts).unwrap(), "[]");
    }
}
