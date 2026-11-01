use crate::domain::{IncidentStatusPhase, Locale};
use crate::email::templates::layout::{self, ButtonStyle, Page, Tone};
use crate::email::templates::{RenderedEmail, SubscriberFooter, single_line};
use crate::i18n::Tr;

/// A public incident update delivered to a confirmed subscriber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncidentMail {
    pub locale: Locale,
    pub page_name: String,
    pub incident_title: String,
    pub phase: String,
    pub message: String,
    pub incident_url: String,
    pub footer: SubscriberFooter,
}

pub fn render(mail: &IncidentMail) -> RenderedEmail {
    let IncidentMail {
        locale,
        page_name,
        incident_title,
        phase,
        message,
        incident_url,
        footer,
    } = mail;
    let tr = Tr::new(*locale);
    // Parsed back to the enum so a new variant has to be classified here
    // rather than defaulting to "still going wrong". That guard is the
    // compiler's, and covers variants only: from_db_str reads an unrecognised
    // phase string as Investigating, and the column's CHECK is what stops one
    // arriving.
    let (label_id, tone) = match IncidentStatusPhase::from_db_str(phase) {
        IncidentStatusPhase::Investigating => ("phase-investigating", Tone::Warn),
        IncidentStatusPhase::Identified => ("phase-identified", Tone::Warn),
        IncidentStatusPhase::Monitoring => ("phase-monitoring", Tone::Warn),
        IncidentStatusPhase::Resolved => ("phase-resolved", Tone::Good),
        IncidentStatusPhase::Postmortem => ("phase-postmortem", Tone::Good),
    };
    let label = tr.t(label_id);
    let subject = format!("[{page_name}] {incident_title} — {label}");

    let text_body = format!(
        "{incident_title}\n\
         {status}\n\
         \n\
         {message}\n\
         \n\
         {view}\n  {incident_url}\n\
         \n\
         {unsubscribe}\n  {unsubscribe_url}\n{credit}",
        status = tr.t_args("email-status-line", [("phase", label.as_str().into())]),
        view = tr.t("email-view-page"),
        unsubscribe = tr.t("email-unsubscribe-text"),
        unsubscribe_url = footer.unsubscribe_url,
        credit = footer.credit_text(tr),
    );

    let mut body = layout::markdown(message);
    body.push_str(&layout::button(
        incident_url,
        &tr.t("email-view-page-button"),
        ButtonStyle::Solid,
    ));

    let html_body = layout::render_in(
        tr.lang(),
        Page {
            title: &subject,
            preheader: &single_line(&crate::markdown::to_plain(message)),
            // A customer's subscribers hear from the page, not from us.
            signature: None,
            header: layout::band(tone, &label.to_uppercase(), incident_title, None),
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

    #[test]
    fn phase_drives_the_status_band_and_the_subject() {
        let r = render(&IncidentMail {
            locale: Locale::En,
            page_name: "Acme status".into(),
            incident_title: "Checkout is failing".into(),
            phase: "investigating".into(),
            message: "We are looking into elevated errors.".into(),
            incident_url: "https://acme.test/incidents/1".into(),
            footer: SubscriberFooter {
                unsubscribe_url: "https://acme.test/subscribe/unsubscribe?s=1&t=2".into(),
                credit_url: None,
            },
        });
        assert_eq!(
            r.subject,
            "[Acme status] Checkout is failing — Investigating"
        );
        assert!(r.html_body.contains("INVESTIGATING"));
        assert!(r.html_body.contains("We are looking into elevated errors."));
        assert!(r.html_body.contains("Unsubscribe"));
    }

    #[test]
    fn a_german_page_reports_in_german() {
        let r = render(&IncidentMail {
            locale: Locale::De,
            page_name: "Acme status".into(),
            incident_title: "Checkout is failing".into(),
            phase: "identified".into(),
            message: "Fix in progress.".into(),
            incident_url: "https://acme.test/incidents/1".into(),
            footer: SubscriberFooter {
                unsubscribe_url: "https://acme.test/subscribe/unsubscribe?s=1&t=2".into(),
                credit_url: None,
            },
        });
        assert_eq!(
            r.subject,
            "[Acme status] Checkout is failing — Ursache erkannt"
        );
        assert!(r.html_body.contains(r#"<html lang="de">"#));
        assert!(r.html_body.contains("URSACHE ERKANNT"));
        assert!(
            r.html_body
                .contains("weil Sie Acme status abonniert haben.")
        );
        assert!(r.text_body.contains("Status: Ursache erkannt"));
        assert!(r.text_body.contains("Abbestellen:"));
        assert!(!r.html_body.contains(['\u{2068}', '\u{E000}']));
    }

    #[test]
    fn the_credit_closes_both_bodies_when_the_page_carries_it() {
        let r = render(&IncidentMail {
            locale: Locale::De,
            page_name: "Acme status".into(),
            incident_title: "Checkout is failing".into(),
            phase: "resolved".into(),
            message: "Fixed.".into(),
            incident_url: "https://acme.test/incidents/1".into(),
            footer: SubscriberFooter {
                unsubscribe_url: "https://acme.test/subscribe/unsubscribe?s=1&t=2".into(),
                credit_url: Some(
                    "https://uptimepage.dev/de/free-status-page?utm_source=status-email&utm_medium=email".into(),
                ),
            },
        });
        let line = "Statusseite von uptimepage – kostenlos für Ihr eigenes Projekt";
        assert!(
            r.text_body.ends_with(&format!(
                "{line}\n  https://uptimepage.dev/de/free-status-page?utm_source=status-email&utm_medium=email\n"
            )),
            "{}",
            r.text_body
        );
        assert!(
            r.html_body.contains(
                "href=\"https://uptimepage.dev/de/free-status-page?utm_source=status-email&amp;utm_medium=email\""
            ),
            "{}",
            r.html_body
        );
        assert!(r.html_body.contains(line));
    }

    #[test]
    fn a_postmortem_does_not_read_as_an_open_problem() {
        let r = render(&IncidentMail {
            locale: Locale::En,
            page_name: "Acme status".into(),
            incident_title: "Checkout is failing".into(),
            phase: "postmortem".into(),
            message: "What happened and what we changed.".into(),
            incident_url: "https://acme.test/incidents/1".into(),
            footer: SubscriberFooter {
                unsubscribe_url: "https://acme.test/subscribe/unsubscribe?s=1&t=2".into(),
                credit_url: None,
            },
        });
        // Tone::Good's signal colour; the warn amber would say "still broken".
        assert!(r.html_body.contains("#43d58f"), "{}", r.html_body);
        assert!(!r.html_body.contains("#f3b94c"));
    }

    #[test]
    fn an_update_written_in_paragraphs_keeps_its_line_breaks() {
        let r = render(&IncidentMail {
            locale: Locale::En,
            page_name: "Acme status".into(),
            incident_title: "Checkout is failing".into(),
            phase: "resolved".into(),
            message: "Root cause found.\nA fix is deployed.".into(),
            incident_url: "https://acme.test/incidents/1".into(),
            footer: SubscriberFooter {
                unsubscribe_url: "https://acme.test/subscribe/unsubscribe?s=1&t=2".into(),
                credit_url: None,
            },
        });
        assert!(
            r.html_body
                .contains("Root cause found.<br>\nA fix is deployed."),
            "{}",
            r.html_body
        );
    }

    #[test]
    fn an_update_in_markdown_is_styled_inline_and_previewed_plain() {
        let r = render(&IncidentMail {
            locale: Locale::En,
            page_name: "Acme status".into(),
            incident_title: "Checkout is failing".into(),
            phase: "identified".into(),
            message: "**Root cause** in [the pool](https://acme.test/pool), see <ticket>.".into(),
            incident_url: "https://acme.test/incidents/1".into(),
            footer: SubscriberFooter {
                unsubscribe_url: "https://acme.test/subscribe/unsubscribe?s=1&t=2".into(),
                credit_url: None,
            },
        });
        let html = &r.html_body;
        assert!(html.contains("<strong>Root cause</strong>"), "{html}");
        assert!(
            html.contains(
                "<a href=\"https://acme.test/pool\" style=\"color:#007a45;text-decoration:underline;\" \
                 rel=\"noopener nofollow\">the pool</a>"
            ),
            "{html}"
        );
        assert!(html.contains("<p style=\"margin:0 0 14px;"), "{html}");
        assert!(html.contains("see &lt;ticket&gt;."), "{html}");
        assert!(
            html.contains("Root cause in the pool, see &lt;ticket&gt;."),
            "the preheader drops the markup: {html}"
        );
    }
}
