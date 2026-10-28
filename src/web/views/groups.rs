//! The group settings page, `/groups/{id}`. Saves run from the page against
//! the JSON API (`/api/v1/groups/{id}`), so this module only renders.

use std::collections::HashMap;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, Query, State};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{OrgId, Target, UserId, WriteSource};
use crate::error::{AppError, codes};
use crate::request::{AuthedBrowser, CurrentOrg};
use crate::storage::TargetFilter;
use crate::templates::filters;
use crate::web::error::WebResult;
use crate::web::views::escalation::{Choice, policy_binding};
use crate::web::views::incidents::members_map;
use crate::web::views::targets_form::{
    ChannelChoice, SecondsChoice, channel_choices, renotify_choices,
};

/// Members listed on the page; the list's own pages show the rest.
const MEMBERS_SHOWN: usize = 500;

pub struct MemberRow {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// Its own channels, kept and muted while the group alerts as one.
    pub channels: Vec<String>,
    /// Its own policy, muted the same way; `None` when it inherits.
    pub policy: Option<String>,
}

pub struct OpenIncident {
    pub id: String,
    pub since: DateTime<Utc>,
    pub acked: bool,
}

pub struct Stamp {
    pub at: DateTime<Utc>,
    pub by: Option<String>,
}

#[derive(Deserialize)]
pub struct SettingsQuery {
    /// The page that linked here, as `incident:<id>` or `monitor:<id>`.
    from: Option<String>,
}

/// Where back, cancel, save and delete return to.
#[derive(Debug, PartialEq)]
pub struct Back {
    pub href: String,
    pub label: &'static str,
    tab: &'static str,
}

impl Back {
    fn from_query(from: Option<&str>) -> Self {
        let linked = from
            .and_then(|f| f.split_once(':'))
            .and_then(|(kind, id)| Some((kind, id.parse::<Uuid>().ok()?)));
        match linked {
            Some(("incident", id)) => Back {
                href: format!("/incidents/{id}"),
                label: "incident",
                tab: "incidents",
            },
            Some(("monitor", id)) => Back {
                href: format!("/targets/{id}/edit"),
                label: "monitor",
                tab: "targets",
            },
            _ => Back {
                href: "/targets".into(),
                label: "monitors",
                tab: "targets",
            },
        }
    }
}

#[derive(Template, WebTemplate)]
#[template(path = "groups/settings.html")]
pub struct GroupSettingsPage {
    pub active_tab: &'static str,
    pub back: Back,
    pub id: String,
    pub name: String,
    pub alert_as_one: bool,
    pub alert_again: bool,
    pub tags: Vec<String>,
    pub tag_options: Vec<String>,
    pub channels: Vec<ChannelChoice>,
    pub renotify: Vec<SecondsChoice>,
    /// `None` off a plan with on-call.
    pub policies: Option<Vec<Choice>>,
    pub policy_hint: String,
    pub members: Vec<MemberRow>,
    pub member_count: usize,
    pub open_incident: Option<OpenIncident>,
    pub created: Stamp,
    pub changed: Stamp,
    /// `app`, `API` or `Terraform`: where it was last changed.
    pub changed_in: &'static str,
    pub managed_by: Option<&'static str>,
}

impl GroupSettingsPage {
    pub fn max_tag_len(&self) -> usize {
        crate::domain::target::MAX_TAG_LEN
    }

    pub fn max_tags(&self) -> usize {
        crate::domain::target::MAX_TAGS_PER_TARGET
    }
}

pub async fn settings(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(q): Query<SettingsQuery>,
) -> WebResult<GroupSettingsPage> {
    let group = state
        .monitor_group_store
        .get(org, id)
        .await?
        .ok_or_else(|| AppError::not_found(codes::GROUP_NOT_FOUND, "group not found"))?;
    let ids = [id];
    let (channels, usage, tags, plan, members) = tokio::try_join!(
        channel_choices(&state, org, &group.alerts),
        state.monitor_group_store.usage(org, &ids),
        state.target_store.list_tags(org, None, 200),
        state.quotas.limit_for_org(org),
        group_members(&state, org, &group.name),
    )?;
    let (people, binding) = tokio::try_join!(
        members_map(&state, org),
        policy_binding(&state, org, group.escalation_policy_id, "group"),
    )?;
    let usage = usage.get(&id).copied().unwrap_or_default();
    let open_incident = usage.open_incident.map(|inc| OpenIncident {
        id: inc.id.to_string(),
        since: inc.started_at,
        acked: inc.acknowledged,
    });
    let mut tag_options: Vec<String> = tags.into_iter().map(|t| t.name).collect();
    for t in &group.tags {
        if !tag_options.contains(t) {
            tag_options.push(t.clone());
        }
    }
    let channel_names: HashMap<String, String> = channels
        .iter()
        .map(|c| (c.id.clone(), format!("{}:{}", c.kind, c.name)))
        .collect();
    let policy_names: HashMap<String, String> = binding
        .choices
        .iter()
        .map(|c| (c.id.clone(), c.name.clone()))
        .collect();
    let member_rows = member_rows(&state, org, &members, &channel_names, &policy_names).await?;
    let stamp = |at, by: Option<UserId>| Stamp {
        at,
        by: by.and_then(|u| people.get(&u).cloned()),
    };
    let back = Back::from_query(q.from.as_deref());
    Ok(GroupSettingsPage {
        active_tab: back.tab,
        back,
        id: id.to_string(),
        alert_as_one: group.alert_as_one,
        alert_again: group.alert_again,
        tags: group.tags.clone(),
        tag_options,
        channels,
        renotify: renotify_choices(group.renotify_interval_secs),
        policies: state
            .cfg
            .on_call_available(&plan)
            .then_some(binding.choices),
        policy_hint: binding.hint,
        member_count: usage.member_count as usize,
        members: member_rows,
        open_incident,
        created: stamp(group.created_at, group.created_by),
        changed: stamp(group.updated_at, group.updated_by),
        changed_in: match group.write_source {
            WriteSource::Ui => "the app",
            WriteSource::Api => "the API",
            WriteSource::Terraform => "Terraform",
        },
        managed_by: group.write_source.managed_label(),
        name: group.name,
    })
}

async fn group_members(
    state: &AppState,
    org: OrgId,
    name: &str,
) -> crate::error::Result<Vec<Target>> {
    let filter = TargetFilter {
        group: Some(name.to_owned()),
        limit: Some(MEMBERS_SHOWN),
        sort: crate::storage::TargetSort::Name,
        ..Default::default()
    };
    state.target_store.list(org, filter).await
}

async fn member_rows(
    state: &AppState,
    org: OrgId,
    members: &[Target],
    channel_names: &HashMap<String, String>,
    policy_names: &HashMap<String, String>,
) -> WebResult<Vec<MemberRow>> {
    let ids: Vec<Uuid> = members.iter().map(|t| t.id).collect();
    let policies = state
        .escalation_policy_store
        .target_policies(org, &ids)
        .await?;
    Ok(members
        .iter()
        .map(|t| MemberRow {
            id: t.id.to_string(),
            name: t.name.clone(),
            enabled: t.enabled,
            channels: t
                .alerts
                .iter()
                .filter_map(|b| channel_names.get(&b.channel_id.to_string()).cloned())
                .collect(),
            policy: policies
                .get(&t.id)
                .and_then(|p| policy_names.get(&p.to_string()).cloned()),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0192f3a4-5b6c-7d8e-9f01-23456789abcd";

    #[test]
    fn back_returns_to_the_page_that_linked_here() {
        assert_eq!(
            Back::from_query(Some(&format!("incident:{ID}"))),
            Back {
                href: format!("/incidents/{ID}"),
                label: "incident",
                tab: "incidents",
            }
        );
        assert_eq!(
            Back::from_query(Some(&format!("monitor:{ID}"))),
            Back {
                href: format!("/targets/{ID}/edit"),
                label: "monitor",
                tab: "targets",
            }
        );
    }

    #[test]
    fn back_falls_back_to_the_list() {
        for from in [
            None,
            Some("incident:nope"),
            Some(&*format!("page:{ID}")),
            Some("//evil.example"),
        ] {
            assert_eq!(Back::from_query(from).href, "/targets", "{from:?}");
        }
    }
}
