use chrono::{DateTime, Utc};

use crate::domain::Locale;
use crate::email::templates::layout::{self, ButtonStyle, Page, Tone};
use crate::email::templates::{RenderedEmail, single_line, subscriber_footnote};
use crate::i18n::Tr;

/// Names a preheader spells out before it counts the rest.
const NAMED_COMPONENTS: usize = 3;

/// One mail about one outage on a page: when it started, or that it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutageMail {
    pub locale: Locale,
    pub page_name: String,
    pub resolved: bool,
    pub components: Vec<OutageComponent>,
    pub page_url: String,
    pub unsubscribe_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutageComponent {
    pub name: String,
    /// The incident's own title, the headline when it is the only one.
    pub title: String,
    pub incident_url: String,
    pub degraded: bool,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

impl OutageMail {
    fn started_at(&self) -> Option<DateTime<Utc>> {
        self.components.iter().map(|c| c.started_at).min()
    }

    fn ended_at(&self) -> Option<DateTime<Utc>> {
        self.components.iter().filter_map(|c| c.ended_at).max()
    }

    fn headline(&self, tr: Tr) -> String {
        match self.components.as_slice() {
            [one] => one.title.clone(),
            many => tr.t_args("email-outage-affected", [("count", many.len().into())]),
        }
    }

    fn names(&self, tr: Tr) -> String {
        let names: Vec<&str> = self.components.iter().map(|c| c.name.as_str()).collect();
        if names.len() <= NAMED_COMPONENTS {
            return names.join(", ");
        }
        tr.t_args(
            "email-outage-names-more",
            [
                ("names", names[..NAMED_COMPONENTS].join(", ").into()),
                ("count", (names.len() - NAMED_COMPONENTS).into()),
            ],
        )
    }
}

fn lasted(from: DateTime<Utc>, to: Option<DateTime<Utc>>) -> i64 {
    to.map_or(0, |to| (to - from).num_seconds().max(0))
}

pub fn render(mail: &OutageMail) -> RenderedEmail {
    let tr = Tr::new(mail.locale);
    let (phase, tone) = if mail.resolved {
        ("phase-resolved", Tone::Good)
    } else {
        ("phase-investigating", Tone::Warn)
    };
    let label = tr.t(phase);
    let headline = mail.headline(tr);
    let subject = format!("[{}] {headline} — {label}", mail.page_name);
    let started = mail.started_at().unwrap_or_default();
    let since = tr.utc_stamp(started);

    let (lead, sub, preheader) = if mail.resolved {
        let ended = mail.ended_at().unwrap_or(started);
        let total = tr.duration(lasted(started, Some(ended)));
        (
            tr.t_args(
                "email-outage-resolved-lead",
                [
                    ("duration", total.clone().into()),
                    ("from", since.clone().into()),
                    ("to", tr.utc_stamp(ended).into()),
                ],
            ),
            format!("{since} · {total}"),
            total,
        )
    } else {
        (
            tr.t_args("email-outage-started-lead", [("at", since.clone().into())]),
            tr.t_args("email-outage-since", [("at", since.into())]),
            mail.names(tr),
        )
    };

    let state = |c: &OutageComponent| {
        if mail.resolved {
            tr.t_args(
                "email-outage-was-down",
                [(
                    "duration",
                    tr.duration(lasted(c.started_at, c.ended_at)).into(),
                )],
            )
        } else if c.degraded {
            tr.t("email-outage-degraded")
        } else {
            tr.t("email-outage-down")
        }
    };
    let rows: Vec<(&str, &str, String)> = mail
        .components
        .iter()
        .map(|c| (c.incident_url.as_str(), c.name.as_str(), state(c)))
        .collect();

    let mut text_lines = String::new();
    for (url, name, right) in &rows {
        text_lines.push_str(&format!("- {name}: {right}\n  {url}\n"));
    }
    let text_body = format!(
        "{headline}\n\
         {status}\n\
         \n\
         {lead}\n\
         \n\
         {text_lines}\n\
         {view}\n  {page_url}\n\
         \n\
         {unsubscribe}\n  {unsubscribe_url}\n",
        status = tr.t_args("email-status-line", [("phase", label.as_str().into())]),
        view = tr.t("email-view-page"),
        page_url = mail.page_url,
        unsubscribe = tr.t("email-unsubscribe-text"),
        unsubscribe_url = mail.unsubscribe_url,
    );

    let mut body = layout::prose(&lead);
    body.push_str(&layout::link_rows(&rows));
    body.push_str(&layout::button(
        &mail.page_url,
        &tr.t("email-view-page-button"),
        ButtonStyle::Solid,
    ));
    let html_body = layout::render_in(
        tr.lang(),
        Page {
            title: &subject,
            preheader: &single_line(&preheader),
            // A customer's subscribers hear from the page, not from us.
            signature: None,
            header: layout::band(tone, &label.to_uppercase(), &headline, Some(&sub)),
            body,
            footnote: Some(subscriber_footnote(
                tr,
                &mail.page_name,
                &mail.unsubscribe_url,
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
    use super::*;

    fn at(hms: &str) -> DateTime<Utc> {
        format!("2026-10-21T{hms}Z").parse().unwrap()
    }

    fn component(name: &str, start: &str, end: Option<&str>) -> OutageComponent {
        OutageComponent {
            name: name.into(),
            title: format!("{name} down"),
            incident_url: format!("https://acme.test/status/incidents/{name}"),
            degraded: false,
            started_at: at(start),
            ended_at: end.map(at),
        }
    }

    fn five(resolved: bool, locale: Locale) -> OutageMail {
        let end = |s: &'static str| resolved.then_some(s);
        OutageMail {
            locale,
            page_name: "Acme status".into(),
            resolved,
            components: vec![
                component("Website", "19:02:26", end("19:04:26")),
                component("API", "19:02:28", end("19:04:28")),
                component("Dashboard", "19:02:30", end("19:04:30")),
                component("Checkout", "19:02:30", end("19:04:30")),
                component("Webhooks", "19:02:37", end("19:04:37")),
            ],
            page_url: "https://acme.test/".into(),
            unsubscribe_url: "https://acme.test/subscribe/unsubscribe?s=1&t=2".into(),
        }
    }

    #[test]
    fn the_started_mail_names_every_component_and_links_each_incident() {
        let r = render(&five(false, Locale::En));
        assert_eq!(
            r.subject,
            "[Acme status] 5 components affected — Investigating"
        );
        assert!(r.html_body.contains("INVESTIGATING"));
        assert!(r.html_body.contains("Since 21 Oct 2026 19:02 UTC"));
        for name in ["Website", "API", "Dashboard", "Checkout", "Webhooks"] {
            assert!(r.html_body.contains(name), "{name}");
            assert!(r.text_body.contains(&format!("- {name}: Down")), "{name}");
        }
        assert!(
            r.html_body
                .contains("https://acme.test/status/incidents/Checkout")
        );
        assert!(r.html_body.contains("Website, API, Dashboard and 2 more"));
        assert!(r.html_body.contains("Unsubscribe"));
    }

    #[test]
    fn the_resolved_mail_says_how_long_overall_and_for_each() {
        let r = render(&five(true, Locale::En));
        assert_eq!(r.subject, "[Acme status] 5 components affected — Resolved");
        assert!(r.html_body.contains("RESOLVED"));
        assert!(
            r.text_body
                .contains("It lasted 2m, from 21 Oct 2026 19:02 UTC to 21 Oct 2026 19:04 UTC."),
            "{}",
            r.text_body
        );
        assert!(r.text_body.contains("- Webhooks: down 2m"));
        assert!(r.html_body.contains("#43d58f"), "good tone");
    }

    #[test]
    fn one_component_is_headed_by_its_own_title() {
        let mut mail = five(false, Locale::En);
        mail.components.truncate(1);
        mail.components[0].degraded = true;
        let r = render(&mail);
        assert_eq!(r.subject, "[Acme status] Website down — Investigating");
        assert!(r.text_body.contains("- Website: Degraded"));
    }

    #[test]
    fn a_german_page_reports_in_german() {
        let r = render(&five(false, Locale::De));
        assert_eq!(
            r.subject,
            "[Acme status] 5 Komponenten betroffen — Untersuchung läuft"
        );
        assert!(r.html_body.contains(r#"<html lang="de">"#));
        assert!(r.text_body.contains("- Website: Ausgefallen"));
        assert!(
            r.html_body
                .contains("Website, API, Dashboard und 2 weitere")
        );
        let resolved = render(&five(true, Locale::De));
        assert!(resolved.subject.ends_with("Behoben"));
        assert!(resolved.text_body.contains("ausgefallen"));
        assert!(!resolved.html_body.contains(['\u{2068}', '\u{E000}']));
    }

    #[test]
    fn names_from_the_customer_are_escaped() {
        let mut mail = five(false, Locale::En);
        mail.components[0].name = "<b>Web</b>".into();
        let r = render(&mail);
        assert!(r.html_body.contains("&lt;b&gt;Web&lt;/b&gt;"));
        assert!(!r.html_body.contains("<b>Web</b>"));
    }
}
