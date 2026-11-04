//! What one notice tells a page's subscribers, snapshotted when the planner
//! creates it, and how one subscriber's mail or POST is built from it. Every
//! subscriber of a notice gets the same words, and a retry renders the same
//! bytes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::{Locale, PageTimezone};
use crate::email::templates::SubscriberFooter;
use crate::email::templates::subscriber_incident::IncidentMail;
use crate::email::templates::subscriber_maintenance::{MaintenanceMail, MaintenanceNotice};
use crate::email::templates::subscriber_outage::{OutageComponent, OutageMail};
use crate::email::{EmailAddress, EmailTemplate, TransactionalEmail};
use crate::public_status::credit;
use crate::storage::page_sql::PageRow;
use crate::storage::subscribers;

/// The deploy's addressing and badge rule, to turn a page into the links its
/// mail carries.
#[derive(Debug, Clone)]
pub struct PageLinks {
    /// Wildcard base domain for `{slug}.{base_domain}` page links; empty on a
    /// self-host deploy, where `public_base_url` is used instead.
    pub base_domain: String,
    pub public_base_url: String,
    /// Whether pages are served on hosts of their own; decides where on the
    /// origin the page itself lives.
    pub subdomain_routes: bool,
    /// Whether a plan without white-label must carry the credit.
    pub enforce_plan_features: bool,
    /// The credit on a page that stored no preference.
    pub default_show_powered_by: bool,
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
        let credit = credit::in_mail(
            page.show_powered_by.unwrap_or(self.default_show_powered_by),
            self.enforce_plan_features,
            page.white_label,
        );
        PageSnapshot {
            name: page.page_name.clone(),
            locale: page.public_locale.clone(),
            timezone: page.public_timezone.clone(),
            origin,
            url,
            credit,
        }
    }
}

fn utc() -> String {
    PageTimezone::default().into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageSnapshot {
    pub name: String,
    pub locale: String,
    /// Absent from a notice planned before pages had a zone.
    #[serde(default = "utc")]
    pub timezone: String,
    pub origin: String,
    pub url: String,
    /// The mail carries the uptimepage credit, as the page's footer does.
    /// Absent from a notice planned before the credit existed.
    #[serde(default)]
    pub credit: bool,
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

    fn credit_url(&self) -> Option<String> {
        self.page.credit.then(|| {
            credit::url(
                self.locale(),
                credit::Surface::Email,
                credit::host_of(&self.page.origin).as_deref(),
            )
        })
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
        let zone = PageTimezone::from_db(&self.page.timezone);
        let page_name = self.page.name.clone();
        let footer = SubscriberFooter {
            unsubscribe_url,
            credit_url: self.credit_url(),
        };
        let template = match &self.body {
            NoticeBody::IncidentUpdate {
                incident_id,
                incident_title,
                phase,
                message,
                ..
            } => EmailTemplate::SubscriberIncident(IncidentMail {
                locale,
                page_name,
                incident_title: incident_title.clone(),
                phase: phase.clone(),
                message: message.clone(),
                incident_url: self.incident_url(*incident_id),
                footer,
            }),
            NoticeBody::Outage {
                resolved,
                components,
            } => EmailTemplate::SubscriberOutage(OutageMail {
                locale,
                zone,
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
                footer,
            }),
            NoticeBody::Maintenance {
                title,
                description,
                phase,
                starts_at,
                ends_at,
                ..
            } => EmailTemplate::SubscriberMaintenance(MaintenanceMail {
                locale,
                zone,
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
                footer,
            }),
            NoticeBody::MaintenanceUpdate {
                title,
                starts_at,
                ends_at,
                phase,
                message,
                ..
            } => EmailTemplate::SubscriberMaintenance(MaintenanceMail {
                locale,
                zone,
                page_name,
                title: title.clone(),
                notice: MaintenanceNotice::Update {
                    message: message.clone(),
                    ended: phase == "completed",
                },
                starts_at: *starts_at,
                ends_at: *ends_at,
                page_url: self.page.url.clone(),
                footer,
            }),
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
            timezone: "UTC".into(),
            origin: "https://acme.example.com".into(),
            url: "https://acme.example.com".into(),
            credit: false,
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

    #[test]
    fn every_notice_keeps_its_action_unsubscribe_and_credit_links() {
        let now = Utc::now();
        let incident_id = Uuid::nil();
        let incident_url = format!("https://acme.example.com/status/incidents/{incident_id}");
        let unsubscribe_url = "https://acme.example.com/subscribe/unsubscribe?s=1&t=2";
        let notices = [
            NoticeBody::IncidentUpdate {
                incident_id,
                incident_title: "API errors".into(),
                update_id: Uuid::nil(),
                phase: "identified".into(),
                message: "Fix rolling out".into(),
            },
            NoticeBody::Outage {
                resolved: false,
                components: vec![ComponentSnapshot {
                    name: "API".into(),
                    title: "API errors".into(),
                    incident_id,
                    degraded: false,
                    started_at: now,
                    ended_at: None,
                }],
            },
            NoticeBody::Maintenance {
                maintenance_id: Uuid::nil(),
                title: "Database upgrade".into(),
                description: None,
                phase: "scheduled".into(),
                starts_at: now,
                ends_at: now,
            },
            NoticeBody::MaintenanceUpdate {
                maintenance_id: Uuid::nil(),
                title: "Database upgrade".into(),
                starts_at: now,
                ends_at: now,
                update_id: Uuid::nil(),
                phase: "completed".into(),
                message: "Upgrade complete".into(),
            },
        ];
        for body in notices {
            let primary_url = if matches!(body, NoticeBody::IncidentUpdate { .. }) {
                incident_url.as_str()
            } else {
                "https://acme.example.com"
            };
            for credit in [false, true] {
                let notice = NoticePayload {
                    page: PageSnapshot { credit, ..page() },
                    body: body.clone(),
                };
                let template = notice
                    .email(
                        EmailAddress::new("status@example.com", "Status"),
                        "a@example.com",
                        unsubscribe_url.into(),
                    )
                    .template;
                assert_eq!(template.primary_url(), Some(primary_url));
                assert_eq!(template.list_unsubscribe_url(), Some(unsubscribe_url));

                let rendered = template.render("Uptimepage");
                assert!(rendered.text_body.contains(unsubscribe_url));
                assert!(
                    rendered
                        .html_body
                        .contains(&unsubscribe_url.replace('&', "&amp;"))
                );
                for body in [&rendered.text_body, &rendered.html_body] {
                    assert!(body.contains(primary_url), "{body}");
                    assert_eq!(body.contains("utm_source=status-email"), credit, "{body}");
                }
            }
        }
    }

    fn row(white_label: bool, show_powered_by: Option<bool>) -> PageRow {
        PageRow {
            page_id: Uuid::nil(),
            org_id: Uuid::nil(),
            page_name: "Acme status".into(),
            public_locale: "en".into(),
            public_timezone: "UTC".into(),
            slug: "acme".into(),
            custom_domain: None,
            custom_domain_published: false,
            show_powered_by,
            white_label,
        }
    }

    fn hosted() -> PageLinks {
        PageLinks {
            base_domain: "example.com".into(),
            public_base_url: "https://app.example.com".into(),
            subdomain_routes: true,
            enforce_plan_features: true,
            default_show_powered_by: true,
        }
    }

    fn rendered(page: PageSnapshot) -> crate::email::templates::RenderedEmail {
        NoticePayload {
            page,
            body: NoticeBody::Maintenance {
                maintenance_id: Uuid::nil(),
                title: "Database upgrade".into(),
                description: None,
                phase: "scheduled".into(),
                starts_at: Utc::now(),
                ends_at: Utc::now(),
            },
        }
        .email(
            EmailAddress::new("status@example.com", "Status"),
            "a@example.com",
            "https://u".into(),
        )
        .template
        .render("Uptimepage")
    }

    #[test]
    fn a_notice_planned_before_pages_had_a_zone_still_sends_in_utc() {
        let stored = serde_json::json!({
            "name": "Acme status",
            "locale": "en",
            "origin": "https://acme.example.com",
            "url": "https://acme.example.com",
            "credit": false,
        });
        let page: PageSnapshot = serde_json::from_value(stored).unwrap();
        assert_eq!(page.timezone, "UTC");
    }

    #[test]
    fn the_snapshot_carries_the_page_zone_into_the_mail() {
        let mut stored = row(true, None);
        stored.public_timezone = "Asia/Tokyo".into();
        let mail = rendered(hosted().snapshot(&stored));
        assert!(mail.text_body.contains(" JST"), "{}", mail.text_body);
        assert!(!mail.text_body.contains(" UTC"), "{}", mail.text_body);
    }

    #[test]
    fn the_mail_carries_the_credit_unless_the_plan_sells_white_label() {
        let free = hosted().snapshot(&row(false, Some(false)));
        assert!(free.credit, "a plan without white-label cannot drop it");
        let mail = rendered(free);
        for body in [&mail.text_body, &mail.html_body] {
            assert!(
                body.contains("Status page by uptimepage — free for your own project"),
                "{body}"
            );
        }
        assert!(mail.text_body.contains(
            "https://uptimepage.dev/free-status-page?utm_source=status-email&utm_medium=email\
             &utm_campaign=footer&utm_content=acme.example.com"
        ));

        let pro = hosted().snapshot(&row(true, Some(false)));
        assert!(!pro.credit);
        let mail = rendered(pro);
        for body in [&mail.text_body, &mail.html_body] {
            assert!(!body.contains("Status page by uptimepage"), "{body}");
            assert!(!body.contains("utm_source=status-email"), "{body}");
        }

        assert!(
            hosted().snapshot(&row(true, None)).credit,
            "white-label follows the page's own choice, the deploy default here"
        );
    }

    #[test]
    fn a_self_hosted_page_mails_without_the_credit() {
        let links = PageLinks {
            enforce_plan_features: false,
            ..hosted()
        };
        assert!(!links.snapshot(&row(false, Some(true))).credit);
    }

    #[test]
    fn a_notice_planned_before_the_credit_goes_without() {
        let mut json = serde_json::to_value(NoticePayload {
            page: hosted().snapshot(&row(false, None)),
            body: NoticeBody::Outage {
                resolved: true,
                components: Vec::new(),
            },
        })
        .unwrap();
        json["page"].as_object_mut().unwrap().remove("credit");
        let back: NoticePayload = serde_json::from_value(json).unwrap();
        assert!(!back.page.credit);
    }
}
