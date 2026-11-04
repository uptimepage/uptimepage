use chrono::{DateTime, Utc};

use crate::domain::{Locale, PageTimezone};
use crate::email::templates::layout::{self, ButtonStyle, Page, Tone};
use crate::email::templates::{RenderedEmail, SubscriberFooter};
use crate::i18n::Tr;

/// What a maintenance email tells a subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaintenanceNotice {
    /// The window was announced, with its description.
    Scheduled { description: Option<String> },
    /// The window ended, with its description.
    Completed { description: Option<String> },
    /// Someone posted an update; `ended` once the window had ended.
    Update { message: String, ended: bool },
}

impl MaintenanceNotice {
    fn heading_key(&self) -> &'static str {
        match self {
            Self::Scheduled { .. } => "email-maintenance-scheduled",
            Self::Completed { .. } => "email-maintenance-completed",
            Self::Update { .. } => "email-maintenance-update",
        }
    }

    /// Whether the window had ended, so its times read as what ran.
    fn ran(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. } | Self::Update { ended: true, .. }
        )
    }

    fn text(&self) -> Option<&str> {
        match self {
            Self::Scheduled { description } | Self::Completed { description } => {
                description.as_deref()
            }
            Self::Update { message, .. } => Some(message),
        }
    }
}

/// A maintenance announcement, completion or posted update for a subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceMail {
    pub locale: Locale,
    pub zone: PageTimezone,
    pub page_name: String,
    pub title: String,
    pub notice: MaintenanceNotice,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub page_url: String,
    pub footer: SubscriberFooter,
}

pub fn render(mail: &MaintenanceMail) -> RenderedEmail {
    let MaintenanceMail {
        locale,
        zone,
        page_name,
        title,
        notice,
        starts_at,
        ends_at,
        page_url,
        footer,
    } = mail;
    let tr = Tr::new(*locale);
    let completed = matches!(notice, MaintenanceNotice::Completed { .. });
    let heading = tr.t(notice.heading_key());
    let subject = format!("[{page_name}] {heading}: {title}");
    let window = format!(
        "{} — {}",
        tr.stamp(*starts_at, *zone),
        tr.stamp(*ends_at, *zone)
    );
    let said = notice.text();
    let desc_text = said.map(|d| format!("\n{d}\n")).unwrap_or_default();

    let text_body = format!(
        "{title}\n\
         {heading}\n\
         {when}\n\
         {desc_text}\n\
         {view}\n  {page_url}\n\
         \n\
         {unsubscribe}\n  {unsubscribe_url}\n{credit}",
        when = tr.t_args(
            "email-maintenance-when",
            [("window", window.as_str().into())]
        ),
        view = tr.t("email-view-page"),
        unsubscribe = tr.t("email-unsubscribe-text"),
        unsubscribe_url = footer.unsubscribe_url,
        credit = footer.credit_text(tr),
    );

    let fact = tr.t(if notice.ran() {
        "email-maintenance-ran"
    } else {
        "email-maintenance-window"
    });
    let mut body = layout::facts(&[(fact.as_str(), window.clone())]);
    if let Some(said) = said {
        body.push_str(&layout::markdown(said));
    }
    body.push_str(&layout::button(
        page_url,
        &tr.t("email-view-page-button"),
        ButtonStyle::Solid,
    ));

    let html_body = layout::render_in(
        tr.lang(),
        Page {
            title: &subject,
            preheader: &window,
            // A customer's subscribers hear from the page, not from us.
            signature: None,
            header: layout::band(
                if completed { Tone::Good } else { Tone::Info },
                &heading.to_uppercase(),
                title,
                Some(&window),
            ),
            body,
            footnote: Some(footer.footnote(tr, page_name)),
        },
    );

    RenderedEmail {
        subject,
        text_body,
        html_body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn rendered(notice: MaintenanceNotice) -> crate::email::templates::RenderedEmail {
        render(&MaintenanceMail {
            locale: Locale::En,
            zone: PageTimezone::default(),
            page_name: "Acme status".into(),
            title: "Database upgrade".into(),
            notice,
            starts_at: Utc.with_ymd_and_hms(2026, 8, 20, 1, 0, 0).unwrap(),
            ends_at: Utc.with_ymd_and_hms(2026, 8, 20, 3, 0, 0).unwrap(),
            page_url: "https://acme.test/".into(),
            footer: SubscriberFooter {
                unsubscribe_url: "https://acme.test/subscribe/unsubscribe?s=1&t=2".into(),
                credit_url: None,
            },
        })
    }

    #[test]
    fn the_window_reads_in_the_page_zone_in_every_body() {
        let r = render(&MaintenanceMail {
            locale: Locale::De,
            zone: PageTimezone::from_db("Europe/Berlin"),
            page_name: "Acme status".into(),
            title: "Database upgrade".into(),
            notice: MaintenanceNotice::Scheduled { description: None },
            starts_at: Utc.with_ymd_and_hms(2026, 10, 25, 0, 30, 0).unwrap(),
            ends_at: Utc.with_ymd_and_hms(2026, 10, 25, 1, 30, 0).unwrap(),
            page_url: "https://acme.test/".into(),
            footer: SubscriberFooter {
                unsubscribe_url: "https://acme.test/subscribe/unsubscribe?s=1&t=2".into(),
                credit_url: None,
            },
        });
        for body in [&r.text_body, &r.html_body] {
            assert!(
                body.contains("25. Okt. 2026, 02:30 MESZ — 25. Okt. 2026, 02:30 MEZ"),
                "each end names its own side of the clock change: {body}"
            );
            assert!(!body.contains("UTC"), "{body}");
        }
    }

    #[test]
    fn the_window_reaches_both_bodies_in_utc() {
        let r = rendered(MaintenanceNotice::Scheduled {
            description: Some("Writes pause for a few minutes.".into()),
        });
        for body in [&r.text_body, &r.html_body] {
            assert!(
                body.contains("20 Aug 2026 01:00 UTC — 20 Aug 2026 03:00 UTC"),
                "window: {body}"
            );
        }
        assert!(r.html_body.contains("SCHEDULED MAINTENANCE"));
    }

    #[test]
    fn a_german_page_dates_the_window_in_german() {
        let r = render(&MaintenanceMail {
            locale: Locale::De,
            zone: PageTimezone::default(),
            page_name: "Acme status".into(),
            title: "Database upgrade".into(),
            notice: MaintenanceNotice::Scheduled { description: None },
            starts_at: Utc.with_ymd_and_hms(2026, 3, 20, 1, 0, 0).unwrap(),
            ends_at: Utc.with_ymd_and_hms(2026, 3, 20, 3, 0, 0).unwrap(),
            page_url: "https://acme.test/".into(),
            footer: SubscriberFooter {
                unsubscribe_url: "https://acme.test/subscribe/unsubscribe?s=1&t=2".into(),
                credit_url: None,
            },
        });
        assert_eq!(
            r.subject,
            "[Acme status] Geplante Wartung: Database upgrade"
        );
        assert!(
            r.text_body
                .contains("Zeitraum: 20. März 2026, 01:00 UTC — 20. März 2026, 03:00 UTC"),
            "{}",
            r.text_body
        );
        assert!(r.html_body.contains("GEPLANTE WARTUNG"));
        assert!(r.html_body.contains(r#"<html lang="de">"#));
    }

    #[test]
    fn an_update_says_what_was_posted_and_a_closing_one_reports_what_ran() {
        let r = rendered(MaintenanceNotice::Update {
            message: "Running late: we now expect to finish by 03:30 UTC.".into(),
            ended: false,
        });
        assert_eq!(
            r.subject,
            "[Acme status] Maintenance update: Database upgrade"
        );
        for body in [&r.text_body, &r.html_body] {
            assert!(body.contains("Running late: we now expect to finish by 03:30 UTC."));
        }
        assert!(r.html_body.contains("WINDOW"));

        let closing = rendered(MaintenanceNotice::Update {
            message: "Finished at 03:24 UTC.".into(),
            ended: true,
        });
        assert!(closing.subject.contains("Maintenance update"));
        assert!(
            closing.html_body.contains("RAN"),
            "a closing note reports what ran"
        );
    }

    #[test]
    fn a_completed_window_is_reported_in_the_past() {
        let r = rendered(MaintenanceNotice::Completed {
            description: Some("Writes pause for a few minutes.".into()),
        });
        assert!(r.subject.contains("Maintenance completed"));
        assert!(r.html_body.contains("MAINTENANCE COMPLETED"));
        assert!(r.html_body.contains("RAN"), "fact labels ship upper-cased");
    }
}
