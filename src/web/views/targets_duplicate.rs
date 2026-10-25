//! The duplicate page: a name and an address for each new monitor, and a plain
//! account of what each one takes from the source. The copy itself is made by
//! the API from the stored row, so nothing secret is rendered here.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::target::DUPLICATE_MAX;
use crate::domain::{CheckSpec, IpVersion, OrgId, Plan, RegionIncidentPolicy, Target, UserId};
use crate::duration::exact_duration;
use crate::error::AppError;
use crate::request::{
    AuthedBrowser, CurrentOrg, CurrentUser, LadderBinding, TokenScopes, ladder_binding,
};
use crate::templates::filters;
use crate::web::error::WebResult;
use crate::web::views::region_display::labeled_regions;
use crate::web::views::targets_form::{KIND_CARDS, http_fields_from};

#[derive(Template, WebTemplate)]
#[template(path = "targets/duplicate.html")]
pub struct DuplicatePage {
    pub active_tab: &'static str,
    pub id: Uuid,
    pub name: String,
    pub kind: &'static str,
    pub address: Option<AddressField>,
    pub carried: Vec<Carried>,
    /// Monitors the plan still has room for, when that can bind before
    /// [`DUPLICATE_MAX`] does.
    pub headroom: Option<i64>,
    pub max_copies: usize,
}

pub struct AddressField {
    pub label: &'static str,
    pub placeholder: &'static str,
    pub source: String,
    /// Names follow a URL's host and path; a host or domain is its own name.
    pub url: bool,
}

pub struct Carried {
    pub label: &'static str,
    pub value: String,
}

impl DuplicatePage {
    fn copy_name(&self) -> String {
        format!("{} (copy)", self.name)
    }

    fn kind_cards(&self) -> Vec<(bool, &'static str, &'static str)> {
        KIND_CARDS
            .iter()
            .map(|(value, label, desc)| (*value == self.kind, *label, *desc))
            .collect()
    }
}

pub async fn page(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    CurrentUser(user): CurrentUser,
    scopes: TokenScopes,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> WebResult<DuplicatePage> {
    let target = state
        .target_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found("TARGET_NOT_FOUND", "monitor not found"))?;
    let usage = state.quotas.account_usage(org).await?;
    let carried = carried(&state, org, user, &scopes, &usage.plan, &target).await?;
    let room = (i64::from(usage.plan.max_targets) - usage.targets).max(0);
    Ok(DuplicatePage {
        active_tab: "targets",
        id: target.id,
        kind: target.check.kind(),
        address: address_field(&target.check),
        name: target.name,
        carried,
        headroom: (room <= DUPLICATE_MAX as i64).then_some(room),
        max_copies: DUPLICATE_MAX,
    })
}

fn address_field(check: &CheckSpec) -> Option<AddressField> {
    let (label, placeholder, url) = match check {
        CheckSpec::Http(_) => ("url", "https://example.com/health", true),
        CheckSpec::Flow(_) => ("start url", "https://app.example.com/login", true),
        CheckSpec::Tcp(_) => ("host or host:port", "db.example.com:5432", false),
        CheckSpec::TlsCert(_) => ("host or host:port", "example.com:443", false),
        CheckSpec::Ping(_) => ("host", "gateway.example.com", false),
        CheckSpec::Dns(_) => ("domain", "api.example.com", false),
        CheckSpec::DomainExpiry(_) => ("domain", "example.com", false),
        CheckSpec::Heartbeat(_) | CheckSpec::Manual(_) => return None,
    };
    Some(AddressField {
        label,
        placeholder,
        source: check.address()?,
        url,
    })
}

async fn carried(
    state: &AppState,
    org: OrgId,
    user: UserId,
    scopes: &TokenScopes,
    plan: &Plan,
    t: &Target,
) -> WebResult<Vec<Carried>> {
    let mut rows = Vec::new();
    let mut push = |label, value: String| {
        if !value.is_empty() {
            rows.push(Carried { label, value });
        }
    };
    push("check", check_detail(&t.check));
    let mut schedule = vec![format!("every {}", exact_duration(t.interval.as_secs()))];
    if !matches!(t.check, CheckSpec::Manual(_)) {
        schedule.push(format!(
            "alerts after {} failed check{}",
            t.alert_confirmations,
            if t.alert_confirmations == 1 { "" } else { "s" }
        ));
    }
    push("schedule", schedule.join(" · "));
    push(
        "state",
        if t.enabled { "running" } else { "paused" }.to_owned(),
    );
    if !t.check.is_passive() {
        let ids = state
            .target_store
            .regions_for_target(org, t.id)
            .await?
            .unwrap_or_default();
        let count = ids.len();
        let catalog = state.regions_detailed().await?;
        let mut regions = labeled_regions(&catalog, ids)
            .into_iter()
            .map(|r| r.label)
            .collect::<Vec<_>>()
            .join(", ");
        if count > 1 {
            regions = format!("{regions} · {}", quorum(t.region_policy));
        }
        push("regions", regions);
    }
    let channels = state.notification_channel_store.list(org).await?;
    let bound: Vec<&str> = t
        .alerts
        .iter()
        .filter_map(|b| channels.iter().find(|c| c.id == b.channel_id))
        .map(|c| c.name.as_str())
        .collect();
    push(
        "alerts",
        if bound.is_empty() {
            "no channel".to_owned()
        } else {
            bound.join(", ")
        },
    );
    let policies = &state.escalation_policy_store;
    let fallback = match policies.org_default(org).await? {
        Some(_) => "; copies follow the org default",
        None => "",
    };
    let escalation = match policies.target_policy(org, t.id).await? {
        Some(own) => {
            match ladder_binding(&state.cfg, state.db.as_ref(), org, user, scopes, plan).await? {
                LadderBinding::NoOnCall => {
                    format!("not copied: escalation comes with the Team plan{fallback}")
                }
                LadderBinding::Denied => {
                    format!("not copied: only the org owner can put monitors on a ladder{fallback}")
                }
                LadderBinding::Allowed => policies
                    .get(org, own)
                    .await?
                    .map(|p| p.name)
                    .unwrap_or_default(),
            }
        }
        None if !fallback.is_empty() => "org default".to_owned(),
        None => String::new(),
    };
    push("escalation", escalation);
    push("tags", t.tags.join(", "));
    push("group", t.group_name.clone().unwrap_or_default());
    Ok(rows)
}

/// Request settings by name only: values and credentials stay server-side.
fn check_detail(check: &CheckSpec) -> String {
    let detail = kind_detail(check);
    let family = check.ip_version();
    if family == IpVersion::Any {
        return detail;
    }
    let pinned = format!("{} only", family.label());
    if detail.is_empty() {
        pinned
    } else {
        format!("{detail} · {pinned}")
    }
}

fn kind_detail(check: &CheckSpec) -> String {
    match check {
        CheckSpec::Http(h) => {
            let fields = http_fields_from(h.clone());
            let mut parts = vec![format!(
                "{} expecting {}",
                fields.method, fields.expected_status_input
            )];
            if !fields.headers.is_empty() {
                let names: Vec<&str> = fields.headers.iter().map(|h| h.name.as_str()).collect();
                parts.push(format!("headers {}", names.join(", ")));
            }
            if h.basic_auth.is_some() {
                parts.push("basic auth".into());
            }
            if h.bearer_token.is_some() {
                parts.push("bearer token".into());
            }
            if h.body.is_some() {
                parts.push("request body".into());
            }
            if h.expected_body_contains.is_some() {
                parts.push("body match".into());
            }
            parts.join(" · ")
        }
        CheckSpec::Dns(d) => format!("{} record", d.record_type.as_str()),
        CheckSpec::Flow(f) => format!(
            "{} step{}",
            f.steps.len(),
            if f.steps.len() == 1 { "" } else { "s" }
        ),
        // Sent as is to every copy, whatever host it is given.
        CheckSpec::TlsCert(c) => c
            .server_name
            .as_ref()
            .map(|sni| format!("sni {sni}"))
            .unwrap_or_default(),
        CheckSpec::Heartbeat(_) | CheckSpec::Manual(_) => check.describe().1,
        CheckSpec::Tcp(_) | CheckSpec::Ping(_) | CheckSpec::DomainExpiry(_) => String::new(),
    }
}

fn quorum(policy: RegionIncidentPolicy) -> String {
    match policy {
        RegionIncidentPolicy::Any => "alerts when any region fails".into(),
        RegionIncidentPolicy::Majority => "alerts when most regions fail".into(),
        RegionIncidentPolicy::All => "alerts when every region fails".into(),
        RegionIncidentPolicy::Count(n) => format!("alerts when {n} regions fail"),
    }
}

#[cfg(test)]
mod tests {
    use askama::Template;

    use super::*;

    fn http() -> CheckSpec {
        serde_json::from_str(
            r#"{"type":"http","url":"https://a.example.com/health","method":"GET","timeout":5000,
                "follow_redirects":false,"max_redirects":0,"expected_status":{"kind":"exact","value":200},
                "headers":{"x-api-key":"k-secret"},"verify_tls":true,"bearer_token":"tok-secret"}"#,
        )
        .unwrap()
    }

    fn page(check: &CheckSpec) -> DuplicatePage {
        DuplicatePage {
            active_tab: "targets",
            id: Uuid::nil(),
            name: "api".into(),
            kind: check.kind(),
            address: address_field(check),
            carried: vec![Carried {
                label: "check",
                value: check_detail(check),
            }],
            headroom: Some(3),
            max_copies: DUPLICATE_MAX,
        }
    }

    #[test]
    fn the_page_names_request_settings_but_never_their_values() {
        let html = page(&http()).render().unwrap();
        assert!(html.contains("headers x-api-key"));
        assert!(html.contains("bearer token"));
        assert!(!html.contains("k-secret"));
        assert!(!html.contains("tok-secret"));
    }

    #[test]
    fn the_first_copy_starts_from_the_source_address() {
        let html = page(&http()).render().unwrap();
        assert!(html.contains(r#"value="https://a.example.com/health""#));
        assert!(html.contains(r#"data-source-address="https://a.example.com/health""#));
        assert!(html.contains(
            r#"data-action="/api/v1/targets/00000000-0000-0000-0000-000000000000/duplicate""#
        ));
        assert!(html.contains("3 monitors left on your plan"));
    }

    #[test]
    fn a_pinned_family_is_named_with_what_the_copies_carry() {
        assert!(!check_detail(&http()).contains("only"));
        let tcp: CheckSpec = serde_json::from_str(
            r#"{"type":"tcp","host":"db.example.com","port":5432,"timeout":3000,"ip_version":"ipv6"}"#,
        )
        .unwrap();
        assert_eq!(check_detail(&tcp), "IPv6 only");
        let CheckSpec::Http(mut h) = http() else {
            unreachable!()
        };
        h.ip_version = IpVersion::Ipv4;
        assert!(check_detail(&CheckSpec::Http(h)).ends_with(" · IPv4 only"));
    }

    #[test]
    fn a_passive_source_asks_for_names_only() {
        let hb: CheckSpec =
            serde_json::from_str(r#"{"type":"heartbeat","period":300000,"grace":60000}"#).unwrap();
        let html = page(&hb).render().unwrap();
        assert!(!html.contains(r#"name="copy_address""#));
        assert!(html.contains(r#"placeholder="api (copy)""#));
    }
}
