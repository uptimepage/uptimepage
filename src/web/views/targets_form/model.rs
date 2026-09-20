//! The form's own model: the whole monitor form as the template reads it, plus
//! every dropdown and picker it offers.

use crate::domain::{CheckSpec, RegionIncidentPolicy};

use super::fields::{
    DnsFields, DomainExpiryFields, FlowFields, HeartbeatFields, HttpFields, PingFields, TcpFields,
    TlsCertFields,
};
use crate::templates::format::exact_duration;

/// One row in the monitor form's Alerts section: an org channel plus whether
/// this monitor binds to it. Channels are pure delivery targets — the firing
/// policy (confirmations, recovery) is monitor-level.
pub struct ChannelChoice {
    pub id: String,
    pub name: String,
    pub kind: &'static str,
    pub selected: bool,
    /// The channel's tag rule as a JSON array. Whether it covers this monitor
    /// is decided in the browser, since the tags are edited on this page, and
    /// a tag may contain a space, so no joined form survives the round trip.
    pub rule_tags: String,
}

pub struct FormModel {
    pub mode: &'static str,
    pub id: String,
    pub action: String,
    pub submit_method: &'static str,
    pub name: String,
    pub interval_s: u64,
    /// The org plan's `min_check_interval_secs`, surfaced so the form's
    /// `min=`/JS guard mirror the same floor the API enforces (no magic 60).
    pub min_interval_s: u64,
    /// Carried over from a real monitor, so the picker must not re-suggest over it.
    pub interval_pinned: bool,
    pub enabled: bool,
    pub tags: Vec<String>,
    /// Free-text operator group label (drives Monitors-page bucketing).
    pub group_name: String,
    /// Existing org group names, offered in the group dropdown.
    pub group_options: Vec<String>,
    /// Existing org tag names, rendered as selectable chips.
    pub tag_options: Vec<String>,
    /// Selected owner user-id (or empty string for "unowned"); the form
    /// renders a `<select>` populated from `owner_options`.
    pub owner_user_id: String,
    /// Org members available as owner candidates.
    pub owner_options: Vec<OwnerChoice>,
    pub check_type: &'static str,
    pub http: HttpFields,
    pub tcp: TcpFields,
    pub ping: PingFields,
    pub heartbeat: HeartbeatFields,
    pub dns: DnsFields,
    pub tls_cert: TlsCertFields,
    pub domain_expiry: DomainExpiryFields,
    pub flow: FlowFields,
    /// The org's notification channels, with this monitor's bindings prefilled.
    pub channels: Vec<ChannelChoice>,
    /// Consecutive failing checks before this monitor alerts (monitor-level).
    pub alert_confirmations: u32,
    /// Whether a recovery is announced to the bound channels (monitor-level).
    pub notify_recovery: bool,
    /// Seconds between outage reminders while unacknowledged; 0 = off.
    pub renotify_interval_secs: u32,
    /// Escalation-policy choices for the monitor's binding selector (edit only;
    /// a not-yet-created monitor has no id to bind). Own binding marked selected.
    pub escalation_choices: Vec<crate::web::views::escalation::Choice>,
    /// What an unbound monitor escalates through — shown while inheriting.
    pub escalation_hint: String,
    /// Whether the escalation-policy section renders at all (off when the
    /// team-paging feature is disabled for the deployment).
    pub show_escalation: bool,
    /// The enabled region catalog grouped by continent, with this monitor's
    /// assignments prefilled (edit only). Empty when single-region.
    pub region_groups: Vec<RegionGroup>,
    /// Detection-threshold dropdown options with the monitor's current one
    /// selected: `any` / `majority` / `all` plus a fixed count up to the catalog.
    pub region_threshold_options: Vec<ThresholdChoice>,
    /// Whether the region assignment + policy section renders (edit only — a
    /// not-yet-created monitor has no id to assign regions to).
    pub show_regions: bool,
    /// Whether the org plan allows a flow monitor; without one its card locks.
    pub flow_available: bool,
}

/// One card in the type rail; `locked` renders inert, with no radio.
pub struct KindCard {
    pub value: &'static str,
    pub label: &'static str,
    pub desc: &'static str,
    pub selected: bool,
    pub locked: bool,
    /// Corner mark; empty renders no badge. `tone` keys the colour ramp.
    pub badge: &'static str,
    pub badge_tone: &'static str,
}

const KIND_CARDS: &[(&str, &str, &str)] = &[
    ("http", "http", "URL / API endpoint"),
    ("tcp", "tcp", "host : port reachable"),
    ("ping", "ping", "icmp echo reply"),
    ("dns", "dns", "record resolves"),
    ("tls_cert", "tls cert", "expiry + chain"),
    ("domain_expiry", "domain", "registration expiry"),
    ("heartbeat", "heartbeat", "your job pings us"),
    ("flow", "flow", "browser login / journey"),
];

/// One option in the detection-threshold dropdown. `value` is what the form
/// submits (`any`/`majority`/`all` or a plain integer).
pub struct ThresholdChoice {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

/// Build the threshold dropdown for `selected`, offering the symbolic presets
/// plus a fixed count for each region up to `max`.
pub(super) fn region_threshold_choices(
    selected: RegionIncidentPolicy,
    max: usize,
) -> Vec<ThresholdChoice> {
    let current = match selected {
        RegionIncidentPolicy::Any => "any".to_string(),
        RegionIncidentPolicy::Majority => "majority".to_string(),
        RegionIncidentPolicy::All => "all".to_string(),
        RegionIncidentPolicy::Count(n) => n.to_string(),
    };
    let mut out = vec![
        ("any", "Any region down".to_string()),
        ("majority", "Majority of regions".to_string()),
        ("all", "All regions".to_string()),
    ]
    .into_iter()
    .map(|(v, label)| ThresholdChoice {
        selected: current == v,
        value: v.to_string(),
        label,
    })
    .collect::<Vec<_>>();
    for n in 1..=max {
        let v = n.to_string();
        out.push(ThresholdChoice {
            selected: current == v,
            value: v.clone(),
            label: format!("{n} region{}", if n == 1 { "" } else { "s" }),
        });
    }
    out
}

/// One preset in the outage-reminder dropdown. `secs` is submitted verbatim.
pub struct RenotifyChoice {
    pub secs: u32,
    pub label: String,
    pub selected: bool,
}

pub struct ConfirmationChoice {
    pub value: u32,
    pub label: String,
    pub selected: bool,
}

/// One preset in the check-interval rail. `secs` is submitted verbatim.
pub struct IntervalChoice {
    pub secs: u64,
    pub label: String,
    pub selected: bool,
}

impl FormModel {
    /// Confirmation-count presets; an off-preset stored value is preserved as its own option.
    pub fn confirmation_options(&self) -> Vec<ConfirmationChoice> {
        let mut values = vec![1, 2, 3, 5];
        if !values.contains(&self.alert_confirmations) {
            values.push(self.alert_confirmations);
            values.sort_unstable();
        }
        values
            .into_iter()
            .map(|value| ConfirmationChoice {
                value,
                label: if value == 1 {
                    "1 fail".to_string()
                } else {
                    format!("{value} fails")
                },
                selected: value == self.alert_confirmations,
            })
            .collect()
    }

    pub fn kind_cards(&self) -> Vec<KindCard> {
        KIND_CARDS
            .iter()
            .map(|(value, label, desc)| {
                // Edit renders the rail static, so a live flow is never locked out.
                let locked = *value == "flow" && !self.flow_available;
                let (badge, badge_tone) = match *value {
                    "flow" if locked => ("coming soon", "warn"),
                    "flow" | "heartbeat" => ("new", "ok"),
                    _ => ("", ""),
                };
                KindCard {
                    value,
                    label,
                    desc,
                    selected: self.check_type == *value,
                    locked,
                    badge,
                    badge_tone,
                }
            })
            .collect()
    }

    /// Whether the selected kind sits on the slow cadence group (the API
    /// enforces an hourly floor for certificate / registration expiry).
    pub fn slow_kind(&self) -> bool {
        crate::domain::min_interval_secs_for_kind(self.check_type) >= 3_600
    }

    /// Per-kind interval floors as a JSON object, mirrored to the client so
    /// the form validates against the same numbers the API enforces.
    pub fn kind_floors_json(&self) -> String {
        let floors: serde_json::Map<String, serde_json::Value> = CheckSpec::ALL_KINDS
            .iter()
            .map(|k| {
                (
                    (*k).to_string(),
                    crate::domain::min_interval_secs_for_kind(k).into(),
                )
            })
            .collect();
        serde_json::Value::Object(floors).to_string()
    }

    /// Mirrored so a kind switch lands where the server would have rendered it.
    pub fn kind_intervals_json(&self) -> String {
        let hints: serde_json::Map<String, serde_json::Value> = CheckSpec::ALL_KINDS
            .iter()
            .map(|k| {
                let h = crate::domain::interval_hints_for_kind(k);
                (
                    (*k).to_string(),
                    serde_json::json!({ "min": h.min, "default": h.default }),
                )
            })
            .collect();
        serde_json::Value::Object(hints).to_string()
    }

    /// Check-interval presets for http/tcp/ping/dns, filtered by the plan floor.
    pub fn interval_options_fast(&self) -> Vec<IntervalChoice> {
        self.interval_group(
            &[30, 60, 120, 300, 600, 900, 1_800, 3_600],
            !self.slow_kind(),
        )
    }

    /// Check-interval presets for tls_cert/domain_expiry.
    pub fn interval_options_slow(&self) -> Vec<IntervalChoice> {
        self.interval_group(&[21_600, 43_200, 86_400], self.slow_kind())
    }

    /// An off-preset stored value is preserved as its own option in the
    /// active group, so editing an API-created monitor keeps its cadence.
    fn interval_group(&self, presets: &[u64], active: bool) -> Vec<IntervalChoice> {
        let picker_min = crate::domain::interval_hints_for_kind(self.check_type).min;
        let mut values: Vec<u64> = presets
            .iter()
            .copied()
            .filter(|s| *s >= self.min_interval_s && (!active || *s >= picker_min))
            .collect();
        if active && !values.contains(&self.interval_s) {
            values.push(self.interval_s);
            values.sort_unstable();
        }
        values
            .into_iter()
            .map(|secs| IntervalChoice {
                secs,
                label: exact_duration(secs),
                selected: active && secs == self.interval_s,
            })
            .collect()
    }

    /// Reminder-cadence presets with the monitor's current interval selected;
    /// an off-preset stored value is preserved as its own option.
    pub fn renotify_options(&self) -> Vec<RenotifyChoice> {
        let mut values: Vec<u32> = vec![0, 900, 1_800, 3_600, 7_200, 21_600];
        if !values.contains(&self.renotify_interval_secs) {
            values.push(self.renotify_interval_secs);
            values.sort_unstable();
        }
        values
            .into_iter()
            .map(|secs| RenotifyChoice {
                secs,
                label: if secs == 0 {
                    "off".to_string()
                } else {
                    exact_duration(u64::from(secs))
                },
                selected: secs == self.renotify_interval_secs,
            })
            .collect()
    }
}

/// One option in the form's "Owner" select.
pub struct OwnerChoice {
    pub id: String,
    pub label: String,
    pub selected: bool,
}

/// One region in the monitor form's assignment checkboxes.
pub struct RegionChoice {
    pub id: String,
    pub label: String,
    pub selected: bool,
    /// Whether this region can run a flow. The picker disables the rest when the
    /// flow kind is selected, since a flow only runs where an engine exists.
    pub flow_capable: bool,
}

/// Regions bucketed under a continent heading for the assignment picker.
pub struct RegionGroup {
    pub label: String,
    pub regions: Vec<RegionChoice>,
}

/// Bucket the region catalog by continent (in `Continent::ALL` order, unknown
/// last as "Other"), marking each as `selected`. Empty buckets are dropped.
pub(super) fn region_groups(
    available: Vec<crate::storage::RegionOption>,
    selected: impl Fn(&str) -> bool,
    flow_capable: &std::collections::HashSet<String>,
) -> Vec<RegionGroup> {
    use crate::domain::region::Continent;
    use crate::web::views::region_display::region_label;
    use std::collections::HashMap;

    let mut by_cont: HashMap<Option<Continent>, Vec<RegionChoice>> = HashMap::new();
    for r in available {
        let cont = r.continent.as_deref().and_then(Continent::parse);
        let choice = RegionChoice {
            selected: selected(&r.id),
            label: region_label(&r),
            flow_capable: flow_capable.contains(&r.id),
            id: r.id,
        };
        by_cont.entry(cont).or_default().push(choice);
    }
    let mut groups = Vec::new();
    for c in Continent::ALL {
        if let Some(regions) = by_cont.remove(&Some(c)) {
            groups.push(RegionGroup {
                label: c.label().to_string(),
                regions,
            });
        }
    }
    if let Some(regions) = by_cont.remove(&None) {
        groups.push(RegionGroup {
            label: "Other".to_string(),
            regions,
        });
    }
    groups
}
