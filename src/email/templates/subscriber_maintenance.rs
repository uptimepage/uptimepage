use chrono::{DateTime, Utc};

use crate::email::templates::layout::{self, ButtonStyle, Page, Tone};
use crate::email::templates::{RenderedEmail, subscriber_credit_text, subscriber_footnote};
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

#[allow(clippy::too_many_arguments)]
pub fn render(
    tr: Tr,
    page_name: &str,
    title: &str,
    notice: &MaintenanceNotice,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
    page_url: &str,
    unsubscribe_url: &str,
    credit_url: Option<&str>,
) -> RenderedEmail {
    let completed = matches!(notice, MaintenanceNotice::Completed { .. });
    let heading = tr.t(notice.heading_key());
    let subject = format!("[{page_name}] {heading}: {title}");
    let window = format!("{} — {}", tr.utc_stamp(starts_at), tr.utc_stamp(ends_at));
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
        credit = subscriber_credit_text(tr, credit_url),
    );

    let fact = tr.t(if notice.ran() {
        "email-maintenance-ran"
    } else {
        "email-maintenance-window"
    });
    let mut body = layout::facts(&[(fact.as_str(), window.clone())]);
    if let Some(said) = said {
        body.push_str(&layout::prose(said));
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
            footnote: Some(subscriber_footnote(
                tr,
                page_name,
                unsubscribe_url,
                credit_url,
            )),
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
    use super::{MaintenanceNotice, render};
    use crate::i18n::Tr;
    use chrono::{TimeZone, Utc};

    fn rendered(notice: MaintenanceNotice) -> crate::email::templates::RenderedEmail {
        render(
            Tr::default(),
            "Acme status",
            "Database upgrade",
            &notice,
            Utc.with_ymd_and_hms(2026, 8, 20, 1, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 8, 20, 3, 0, 0).unwrap(),
            "https://acme.test/",
            "https://acme.test/subscribe/unsubscribe?s=1&t=2",
            None,
        )
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
        let r = render(
            Tr::new(crate::domain::Locale::De),
            "Acme status",
            "Database upgrade",
            &MaintenanceNotice::Scheduled { description: None },
            Utc.with_ymd_and_hms(2026, 3, 20, 1, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 3, 20, 3, 0, 0).unwrap(),
            "https://acme.test/",
            "https://acme.test/subscribe/unsubscribe?s=1&t=2",
            None,
        );
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
