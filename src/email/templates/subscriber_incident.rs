use crate::domain::IncidentStatusPhase;
use crate::email::templates::layout::{self, ButtonStyle, Page, Tone};
use crate::email::templates::{
    RenderedEmail, single_line, subscriber_credit_text, subscriber_footnote,
};
use crate::i18n::Tr;

#[allow(clippy::too_many_arguments)]
pub fn render(
    tr: Tr,
    page_name: &str,
    incident_title: &str,
    phase: &str,
    message: &str,
    incident_url: &str,
    unsubscribe_url: &str,
    credit_url: Option<&str>,
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
         {unsubscribe}\n  {unsubscribe_url}\n{credit}",
        status = tr.t_args("email-status-line", [("phase", label.as_str().into())]),
        view = tr.t("email-view-page"),
        unsubscribe = tr.t("email-unsubscribe-text"),
        credit = subscriber_credit_text(tr, credit_url),
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
            None,
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
            None,
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
    fn the_credit_closes_both_bodies_when_the_page_carries_it() {
        let r = render(
            Tr::new(crate::domain::Locale::De),
            "Acme status",
            "Checkout is failing",
            "resolved",
            "Fixed.",
            "https://acme.test/incidents/1",
            "https://acme.test/subscribe/unsubscribe?s=1&t=2",
            Some(
                "https://uptimepage.dev/de/free-status-page?utm_source=status-email&utm_medium=email",
            ),
        );
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
        let r = render(
            Tr::default(),
            "Acme status",
            "Checkout is failing",
            "postmortem",
            "What happened and what we changed.",
            "https://acme.test/incidents/1",
            "https://acme.test/subscribe/unsubscribe?s=1&t=2",
            None,
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
            None,
        );
        assert!(
            r.html_body
                .contains("Root cause found.<br>\nA fix is deployed."),
            "{}",
            r.html_body
        );
    }

    #[test]
    fn an_update_in_markdown_is_styled_inline_and_previewed_plain() {
        let r = render(
            Tr::default(),
            "Acme status",
            "Checkout is failing",
            "identified",
            "**Root cause** in [the pool](https://acme.test/pool), see <ticket>.",
            "https://acme.test/incidents/1",
            "https://acme.test/subscribe/unsubscribe?s=1&t=2",
            None,
        );
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
