//! How every incident view names who acted: acknowledgers, timeline actors
//! and update authors.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::domain::{ActorType, IncidentAcknowledgement, IncidentEvent, UserId};

use super::{OwnerAvatar, member_avatar};

/// Someone who acknowledged, named the same way on every incident view.
#[derive(Clone)]
pub struct AckerView {
    /// "alice@example.com", "notification", "Telegram" or "former member".
    pub name: String,
    /// "by alice@example.com", "by alice@example.com via Telegram",
    /// "via notification", "via Telegram" or "by former member".
    pub phrase: String,
    pub avatar: Option<OwnerAvatar>,
    pub at: DateTime<Utc>,
}

pub(super) fn acker_views(
    acks: &[IncidentAcknowledgement],
    members: &HashMap<UserId, String>,
) -> Vec<AckerView> {
    acks.iter()
        .map(|a| {
            let (name, phrase) = if a.anonymous {
                let name = unnamed_actor(a.actor_type);
                let phrase = format!("via {name}");
                (name, phrase)
            } else {
                let name = member_name(a.actor_id, members);
                let phrase = match a.actor_type.via() {
                    Some(via) => format!("by {name} via {via}"),
                    None => format!("by {name}"),
                };
                (name, phrase)
            };
            AckerView {
                avatar: a.actor_id.and_then(|u| member_avatar(u, members)),
                name,
                phrase,
                at: a.at,
            }
        })
        .collect()
}

/// Whether `viewer` is already among those who acknowledged, on the web or
/// through MCP, so their own acknowledge button can go.
pub(super) fn viewer_acknowledged(acks: &[IncidentAcknowledgement], viewer: UserId) -> bool {
    acks.iter().any(|a| a.actor_id == Some(viewer))
}

/// Everyone who acknowledged an incident's current episode, first (credited)
/// first, and whether the viewer is one of them. Built together so a view
/// cannot show the list without deciding the viewer's button.
#[derive(Clone, Default)]
pub struct AckList {
    pub ackers: Vec<AckerView>,
    pub mine: bool,
}

pub(crate) fn ack_list(
    acks: &[IncidentAcknowledgement],
    viewer: UserId,
    members: &HashMap<UserId, String>,
) -> AckList {
    AckList {
        ackers: acker_views(acks, members),
        mine: viewer_acknowledged(acks, viewer),
    }
}

/// Stored author (user id / `system` / NULL) → label; a departed member reads
/// `former member`, not a raw id.
pub(super) fn author_label(author: Option<&str>, members: &HashMap<UserId, String>) -> String {
    match author {
        None | Some("system") => "system".to_string(),
        Some(s) => match Uuid::parse_str(s) {
            Ok(u) => members
                .get(&UserId(u))
                .cloned()
                .unwrap_or_else(|| "former member".to_string()),
            Err(_) => s.to_string(),
        },
    }
}

/// Resolve an event's actor to a human label, plus where a named member acted
/// when it was not the console. The timeline keeps no anonymous flag, so an
/// app actor without an id reads as unlinked even when a member since deleted
/// was behind it.
pub(super) fn actor_label(
    e: &IncidentEvent,
    members: &HashMap<UserId, String>,
) -> (String, Option<&'static str>) {
    match e.actor_type {
        ActorType::User | ActorType::Mcp => (member_name(e.actor_id, members), e.actor_type.via()),
        ActorType::Telegram | ActorType::Pushover if e.actor_id.is_some() => {
            (member_name(e.actor_id, members), e.actor_type.via())
        }
        other => (unnamed_actor(other), None),
    }
}

/// A member by email. An account deleted since leaves no id behind, and one
/// that left the org no email, so both read as a former member.
fn member_name(actor_id: Option<UserId>, members: &HashMap<UserId, String>) -> String {
    actor_id
        .and_then(|u| members.get(&u).cloned())
        .unwrap_or_else(|| "former member".to_string())
}

/// What acted when no member is named: the system, a notification link, or
/// an app account nobody linked.
fn unnamed_actor(actor_type: ActorType) -> String {
    match actor_type {
        ActorType::System => "system".to_string(),
        // Whoever held the notification; the event message says which one.
        ActorType::Link => "notification".to_string(),
        app => app.via().unwrap_or("notification").to_string(),
    }
}
