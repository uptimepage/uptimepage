use chrono::{DateTime, Utc};

use crate::email::templates::layout::{self, ButtonStyle, Page, Tone};
use crate::email::templates::{RenderedEmail, subscriber_footnote};
use crate::i18n::Tr;

#[allow(clippy::too_many_arguments)]
pub fn render(
    tr: Tr,
    page_name: &str,
    title: &str,
    description: Option<&str>,
    phase: &str,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
    page_url: &str,
    unsubscribe_url: &str,
) -> RenderedEmail {
    let completed = phase == "completed";
    let heading = tr.t(if completed {
        "email-maintenance-completed"
    } else {
        "email-maintenance-scheduled"
    });
    let subject = format!("[{page_name}] {heading}: {title}");
    let window = format!("{} — {}", tr.utc_stamp(starts_at), tr.utc_stamp(ends_at));
    let desc_text = description.map(|d| format!("\n{d}\n")).unwrap_or_default();

    let text_body = format!(
        "{title}\n\
         {heading}\n\
         {when}\n\
         {desc_text}\n\
         {view}\n  {page_url}\n\
         \n\
         {unsubscribe}\n  {unsubscribe_url}\n",
        when = tr.t_args(
            "email-maintenance-when",
            [("window", window.as_str().into())]
        ),
        view = tr.t("email-view-page"),
        unsubscribe = tr.t("email-unsubscribe-text"),
    );

    let fact = tr.t(if completed {
        "email-maintenance-ran"
    } else {
        "email-maintenance-window"
    });
    let mut body = layout::facts(&[(fact.as_str(), window.clone())]);
    if let Some(description) = description {
        body.push_str(&layout::prose(description));
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
    use chrono::{TimeZone, Utc};

    fn rendered(phase: &str) -> crate::email::templates::RenderedEmail {
        render(
            Tr::default(),
            "Acme status",
            "Database upgrade",
            Some("Writes pause for a few minutes."),
            phase,
            Utc.with_ymd_and_hms(2026, 8, 20, 1, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 8, 20, 3, 0, 0).unwrap(),
            "https://acme.test/",
            "https://acme.test/subscribe/unsubscribe?s=1&t=2",
        )
    }

    #[test]
    fn the_window_reaches_both_bodies_in_utc() {
        let r = rendered("scheduled");
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
            None,
            "scheduled",
            Utc.with_ymd_and_hms(2026, 3, 20, 1, 0, 0).unwrap(),
            Utc.with_ymd_and_hms(2026, 3, 20, 3, 0, 0).unwrap(),
            "https://acme.test/",
            "https://acme.test/subscribe/unsubscribe?s=1&t=2",
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
    fn a_completed_window_is_reported_in_the_past() {
        let r = rendered("completed");
        assert!(r.subject.contains("Maintenance completed"));
        assert!(r.html_body.contains("MAINTENANCE COMPLETED"));
        assert!(r.html_body.contains("RAN"), "fact labels ship upper-cased");
    }
}
