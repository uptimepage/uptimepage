use crate::domain::IncidentStatusPhase;
use crate::email::templates::layout::{self, ButtonStyle, Page, Tone};
use crate::email::templates::{RenderedEmail, single_line, subscriber_footnote};
use crate::i18n::Tr;

pub fn render(
    tr: Tr,
    page_name: &str,
    incident_title: &str,
    phase: &str,
    message: &str,
    incident_url: &str,
    unsubscribe_url: &str,
) -> RenderedEmail {
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
         {unsubscribe}\n  {unsubscribe_url}\n",
        status = tr.t_args("email-status-line", [("phase", label.as_str().into())]),
        view = tr.t("email-view-page"),
        unsubscribe = tr.t("email-unsubscribe-text"),
    );

    let mut body = layout::prose(message);
    body.push_str(&layout::button(
        incident_url,
        &tr.t("email-view-page-button"),
        ButtonStyle::Solid,
    ));

    let html_body = layout::render_in(
        tr.lang(),
        Page {
            title: &subject,
            preheader: &single_line(message),
            // A customer's subscribers hear from the page, not from us.
            signature: None,
            header: layout::band(tone, &label.to_uppercase(), incident_title, None),
            body,
            footnote: Some(subscriber_footnote(tr, page_name, unsubscribe_url)),
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
    use super::render;
    use crate::i18n::Tr;

    #[test]
    fn phase_drives_the_status_band_and_the_subject() {
        let r = render(
            Tr::default(),
            "Acme status",
            "Checkout is failing",
            "investigating",
            "We are looking into elevated errors.",
            "https://acme.test/incidents/1",
            "https://acme.test/subscribe/unsubscribe?s=1&t=2",
        );
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
        let r = render(
            Tr::new(crate::domain::Locale::De),
            "Acme status",
            "Checkout is failing",
            "identified",
            "Fix in progress.",
            "https://acme.test/incidents/1",
            "https://acme.test/subscribe/unsubscribe?s=1&t=2",
        );
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
    fn a_postmortem_does_not_read_as_an_open_problem() {
        let r = render(
            Tr::default(),
            "Acme status",
            "Checkout is failing",
            "postmortem",
            "What happened and what we changed.",
            "https://acme.test/incidents/1",
            "https://acme.test/subscribe/unsubscribe?s=1&t=2",
        );
        // Tone::Good's signal colour; the warn amber would say "still broken".
        assert!(r.html_body.contains("#43d58f"), "{}", r.html_body);
        assert!(!r.html_body.contains("#f3b94c"));
    }

    #[test]
    fn an_update_written_in_paragraphs_keeps_its_line_breaks() {
        let r = render(
            Tr::default(),
            "Acme status",
            "Checkout is failing",
            "resolved",
            "Root cause found.\nA fix is deployed.",
            "https://acme.test/incidents/1",
            "https://acme.test/subscribe/unsubscribe?s=1&t=2",
        );
        assert!(
            r.html_body
                .contains("Root cause found.<br>A fix is deployed.")
        );
    }
}
