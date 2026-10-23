//! What one notice tells a page's subscribers, snapshotted when the planner
//! creates it, and how one subscriber's mail or POST is built from it. Every
//! subscriber of a notice gets the same words, and a retry renders the same
//! bytes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::Locale;
use crate::email::templates::subscriber_maintenance::MaintenanceNotice;
use crate::email::templates::subscriber_outage::{OutageComponent, OutageMail};
use crate::email::{EmailAddress, EmailTemplate, TransactionalEmail};
use crate::storage::page_sql::PageRow;
use crate::storage::subscribers;

/// The deploy's addressing, to turn a page into the links its mail carries.
#[derive(Debug, Clone)]
pub struct PageLinks {
    /// Wildcard base domain for `{slug}.{base_domain}` page links; empty on a
    /// self-host deploy, where `public_base_url` is used instead.
    pub base_domain: String,
    pub public_base_url: String,
    /// Whether pages are served on hosts of their own; decides where on the
    /// origin the page itself lives.
    pub subdomain_routes: bool,
}

impl PageLinks {
    pub fn snapshot(&self, page: &PageRow) -> PageSnapshot {
        let origin = crate::public_status::urls::page_origin(
            &self.base_domain,
            &self.public_base_url,
            &page.slug,
            page.custom_domain.as_deref(),
            page.custom_domain_published,
        );
        // The page's own address: subscribers keep it, so it has to be the
        // URL the page answers on rather than one that redirects there.
        let url = crate::public_status::urls::status_url_for(self.subdomain_routes, &origin);
        PageSnapshot {
            name: page.page_name.clone(),
            locale: page.public_locale.clone(),
            origin,
            url,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageSnapshot {
    pub name: String,
    pub locale: String,
    pub origin: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentSnapshot {
    pub name: String,
    pub title: String,
    pub incident_id: Uuid,
    pub degraded: bool,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NoticeBody {
    IncidentUpdate {
        incident_id: Uuid,
        incident_title: String,
        update_id: Uuid,
        phase: String,
        message: String,
    },
    Outage {
        resolved: bool,
        components: Vec<ComponentSnapshot>,
    },
    Maintenance {
        maintenance_id: Uuid,
        title: String,
        description: Option<String>,
        phase: String,
        starts_at: DateTime<Utc>,
        ends_at: DateTime<Utc>,
    },
    MaintenanceUpdate {
        maintenance_id: Uuid,
        title: String,
        starts_at: DateTime<Utc>,
        ends_at: DateTime<Utc>,
        update_id: Uuid,
        phase: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoticePayload {
    pub page: PageSnapshot,
    #[serde(flatten)]
    pub body: NoticeBody,
}

impl NoticePayload {
    fn locale(&self) -> Locale {
        Locale::from_db(&self.page.locale)
    }

    fn incident_url(&self, incident_id: Uuid) -> String {
        format!("{}/status/incidents/{incident_id}", self.page.origin)
    }

    pub fn unsubscribe_url(&self, secret: &str, subscriber_id: Uuid) -> String {
        subscribers::unsubscribe_url(secret, &self.page.origin, subscriber_id)
    }

    /// The mail one subscriber gets.
    pub fn email(
        &self,
        from: EmailAddress,
        to: &str,
        unsubscribe_url: String,
    ) -> TransactionalEmail {
        let locale = self.locale();
        let page_name = self.page.name.clone();
        let template = match &self.body {
            NoticeBody::IncidentUpdate {
                incident_id,
                incident_title,
                phase,
                message,
                ..
            } => EmailTemplate::SubscriberIncident {
                locale,
                page_name,
                incident_title: incident_title.clone(),
                phase: phase.clone(),
                message: message.clone(),
                incident_url: self.incident_url(*incident_id),
                unsubscribe_url,
            },
            NoticeBody::Outage {
                resolved,
                components,
            } => EmailTemplate::SubscriberOutage(OutageMail {
                locale,
                page_name,
                resolved: *resolved,
                components: components
                    .iter()
                    .map(|c| OutageComponent {
                        name: c.name.clone(),
                        title: c.title.clone(),
                        incident_url: self.incident_url(c.incident_id),
                        degraded: c.degraded,
                        started_at: c.started_at,
                        ended_at: c.ended_at,
                    })
                    .collect(),
                page_url: self.page.url.clone(),
                unsubscribe_url,
            }),
            NoticeBody::Maintenance {
                title,
                description,
                phase,
                starts_at,
                ends_at,
                ..
            } => EmailTemplate::SubscriberMaintenance {
                locale,
                page_name,
                title: title.clone(),
                notice: if phase == "completed" {
                    MaintenanceNotice::Completed {
                        description: description.clone(),
                    }
                } else {
                    MaintenanceNotice::Scheduled {
                        description: description.clone(),
                    }
                },
                starts_at: *starts_at,
                ends_at: *ends_at,
                page_url: self.page.url.clone(),
                unsubscribe_url,
            },
            NoticeBody::MaintenanceUpdate {
                title,
                starts_at,
                ends_at,
                phase,
                message,
                ..
            } => EmailTemplate::SubscriberMaintenance {
                locale,
                page_name,
                title: title.clone(),
                notice: MaintenanceNotice::Update {
                    message: message.clone(),
                    ended: phase == "completed",
                },
                starts_at: *starts_at,
                ends_at: *ends_at,
                page_url: self.page.url.clone(),
                unsubscribe_url,
            },
        };
        TransactionalEmail {
            from,
            to: EmailAddress::new(to, to),
            template,
        }
    }

    /// The JSON one webhook subscriber gets. `None` for an outage: outages
    /// are email only, and a program follows each incident's own updates.
    pub fn webhook(&self, unsubscribe_url: &str) -> Option<serde_json::Value> {
        let page = serde_json::json!({ "name": self.page.name, "url": self.page.url });
        Some(match &self.body {
            NoticeBody::IncidentUpdate {
                incident_id,
                incident_title,
                update_id,
                phase,
                message,
            } => serde_json::json!({
                "type": "incident_update",
                "incident": { "id": incident_id, "title": incident_title },
                "update": { "id": update_id, "phase": phase, "message": message },
                "page": page,
                "incident_url": self.incident_url(*incident_id),
                "unsubscribe_url": unsubscribe_url,
            }),
            NoticeBody::Outage { .. } => return None,
            NoticeBody::Maintenance {
                maintenance_id,
                title,
                description,
                phase,
                starts_at,
                ends_at,
            } => serde_json::json!({
                "type": "maintenance",
                "maintenance": {
                    "id": maintenance_id,
                    "title": title,
                    "description": description,
                    "phase": phase,
                    "starts_at": starts_at,
                    "ends_at": ends_at,
                },
                "page": page,
                "unsubscribe_url": unsubscribe_url,
            }),
            NoticeBody::MaintenanceUpdate {
                maintenance_id,
                title,
                starts_at,
                ends_at,
                update_id,
                phase,
                message,
            } => serde_json::json!({
                "type": "maintenance_update",
                "maintenance": {
                    "id": maintenance_id,
                    "title": title,
                    "starts_at": starts_at,
                    "ends_at": ends_at,
                },
                "update": { "id": update_id, "phase": phase, "message": message },
                "page": page,
                "unsubscribe_url": unsubscribe_url,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> PageSnapshot {
        PageSnapshot {
            name: "Acme status".into(),
            locale: "en".into(),
            origin: "https://acme.example.com".into(),
            url: "https://acme.example.com".into(),
        }
    }

    #[test]
    fn a_payload_survives_its_round_trip_through_the_database() {
        let payload = NoticePayload {
            page: page(),
            body: NoticeBody::Outage {
                resolved: false,
                components: vec![ComponentSnapshot {
                    name: "API".into(),
                    title: "API down".into(),
                    incident_id: Uuid::nil(),
                    degraded: false,
                    started_at: Utc::now(),
                    ended_at: None,
                }],
            },
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["type"], "outage");
        let back: NoticePayload = serde_json::from_value(json).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn an_outage_is_mail_only_and_an_update_keeps_its_webhook_shape() {
        let outage = NoticePayload {
            page: page(),
            body: NoticeBody::Outage {
                resolved: true,
                components: Vec::new(),
            },
        };
        assert!(outage.webhook("https://u").is_none());

        let update = NoticePayload {
            page: page(),
            body: NoticeBody::IncidentUpdate {
                incident_id: Uuid::nil(),
                incident_title: "API errors".into(),
                update_id: Uuid::nil(),
                phase: "identified".into(),
                message: "Fix rolling out".into(),
            },
        };
        let json = update.webhook("https://u").unwrap();
        assert_eq!(json["type"], "incident_update");
        assert_eq!(json["incident"]["title"], "API errors");
        assert_eq!(json["page"]["url"], "https://acme.example.com");
        assert_eq!(
            json["incident_url"],
            format!("https://acme.example.com/status/incidents/{}", Uuid::nil())
        );
        let mail = update.email(
            EmailAddress::new("status@example.com", "Status"),
            "a@example.com",
            "https://u".into(),
        );
        assert_eq!(mail.template.list_unsubscribe_url(), Some("https://u"));
    }
}
