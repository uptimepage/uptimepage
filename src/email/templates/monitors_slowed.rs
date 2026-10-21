use crate::domain::long_down::LONG_DOWN;
use crate::email::templates::layout::{self, Page, Tone};
use crate::email::templates::{RenderedEmail, html_escape, single_line};

#[derive(Debug, Clone)]
pub struct SlowedMonitor {
    pub name: String,
    pub org_name: Option<String>,
    /// The monitor's page, where Pause and Delete are. A one-click pause link
    /// would be fired by link scanners.
    pub url: Option<String>,
}

const STILL_NEEDED: &str = "Fix it, then run a check from its page. A passing check puts it \
                            straight back on its own interval.";
const NOT_NEEDED: &str = "Pause or delete it on its page. Test servers that were switched off \
                          and sites that moved are the usual reasons.";

pub fn render(
    site_name: &str,
    monitors: &[SlowedMonitor],
    docs_url: Option<&str>,
) -> RenderedEmail {
    let after = LONG_DOWN.after_words();
    let every = LONG_DOWN.interval_words();
    let (subject, headline) = match monitors {
        [one] => (
            format!("Do you still need \"{}\"?", single_line(&one.name)),
            format!("{} has been down for {after}", single_line(&one.name)),
        ),
        [first, rest @ ..] => (
            format!(
                "Do you still need {} and {} more?",
                single_line(&first.name),
                rest.len()
            ),
            format!("{} monitors have been down for {after}", monitors.len()),
        ),
        [] => (String::new(), String::new()),
    };
    let preheader = format!("Down from every region for {after}, so we now check every {every}.");
    let until = if monitors.len() == 1 {
        format!("Until it answers again, we check it every {every} instead of on its own interval.")
    } else {
        format!(
            "Until a monitor answers again, we check it every {every} instead of on its own \
             interval."
        )
    };

    let mut text_body = String::new();
    let mut body = String::new();
    match monitors {
        [one] => {
            text_body.push_str(&format!(
                "{}{} has failed every check from every region for {after}. {until}\n",
                one.name,
                in_org(one.org_name.as_deref())
            ));
            body.push_str(&layout::paragraph(&format!(
                "{}{} has failed every check from every region for <strong>{after}</strong>. {until}",
                name_html(one),
                html_escape(&in_org(one.org_name.as_deref())),
            )));
            if let Some(url) = &one.url {
                text_body.push_str(&format!("\n  {url}\n"));
            }
        }
        _ => {
            let lead = format!(
                "These {} monitors have failed every check from every region for",
                monitors.len()
            );
            text_body.push_str(&format!("{lead} {after}:\n\n"));
            body.push_str(&layout::paragraph(&format!(
                "{lead} <strong>{after}</strong>:"
            )));
            for m in monitors {
                let org = in_org(m.org_name.as_deref());
                text_body.push_str(&format!("- {}{org}\n", m.name));
                if let Some(url) = &m.url {
                    text_body.push_str(&format!("  {url}\n"));
                }
                body.push_str(&layout::paragraph(&format!(
                    "{}{}",
                    name_html(m),
                    html_escape(&org)
                )));
            }
            text_body.push_str(&format!("\n{until}\n"));
            body.push_str(&layout::paragraph(&until));
        }
    }
    text_body.push_str(&format!(
        "\nStill needed? {STILL_NEEDED}\n\nNot needed any more? {NOT_NEEDED}\n"
    ));
    body.push_str(&layout::paragraph(&format!(
        "<strong>Still needed?</strong> {STILL_NEEDED}"
    )));
    body.push_str(&layout::paragraph(&format!(
        "<strong>Not needed any more?</strong> {NOT_NEEDED}"
    )));
    if let Some(url) = docs_url {
        text_body.push_str(&format!("\nHow long-down monitors work:\n\n  {url}\n"));
        body.push_str(&layout::paragraph(&layout::quiet_link(
            url,
            "How long-down monitors work",
        )));
    }

    let html_body = layout::render(Page {
        title: &subject,
        preheader: &preheader,
        signature: Some(site_name),
        header: layout::band(
            Tone::Info,
            "checked less often",
            &headline,
            Some(&format!("now every {every} from each region")),
        ),
        body,
        footnote: Some(layout::fine_print(footnote(monitors.len()))),
    });

    RenderedEmail {
        subject,
        text_body,
        html_body,
    }
}

fn name_html(m: &SlowedMonitor) -> String {
    match &m.url {
        Some(url) => layout::link(url, &m.name),
        None => format!("<strong>{}</strong>", html_escape(&m.name)),
    }
}

fn footnote(count: usize) -> &'static str {
    if count == 1 {
        "You are receiving this as the owner of this monitor or its organization. \
         We send it once each time a monitor is slowed."
    } else {
        "You are receiving this as the owner of these monitors or their organization. \
         We send it once each time a monitor is slowed."
    }
}

fn in_org(org_name: Option<&str>) -> String {
    org_name.map(|o| format!(" in {o}")).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{SlowedMonitor, render};
    use crate::domain::long_down::LONG_DOWN;

    fn monitor(name: &str) -> SlowedMonitor {
        SlowedMonitor {
            name: name.into(),
            org_name: Some("Acme Inc".into()),
            url: Some(format!("https://app.test/targets/{name}")),
        }
    }

    #[test]
    fn the_subject_asks_whether_the_monitors_are_still_needed() {
        let one = render("Uptimepage", &[monitor("staging api")], None);
        assert_eq!(one.subject, "Do you still need \"staging api\"?");
        let three = [
            monitor("staging-api"),
            monitor("staging-web"),
            monitor("db"),
        ];
        let many = render("Uptimepage", &three, None);
        assert_eq!(many.subject, "Do you still need staging-api and 2 more?");
    }

    #[test]
    fn several_monitors_share_one_mail_each_with_its_link() {
        let monitors = [monitor("staging-api"), monitor("staging-web")];
        let out = render(
            "Uptimepage",
            &monitors,
            Some("https://docs.test/monitor-types"),
        );
        for body in [&out.text_body, &out.html_body] {
            for m in &monitors {
                assert!(body.contains(m.url.as_deref().unwrap()));
            }
            assert!(body.contains("Acme Inc"));
            assert!(body.contains("https://docs.test/monitor-types"));
        }
    }

    #[test]
    fn the_copy_renders_the_long_down_values() {
        let out = render("Uptimepage", &[monitor("a"), monitor("b")], None);
        for body in [&out.text_body, &out.html_body] {
            assert!(body.contains(&format!("for {}", LONG_DOWN.after_words())));
            assert!(body.contains(&format!("every {}", LONG_DOWN.interval_words())));
            assert!(body.contains("run a check"));
            assert!(body.contains("Still needed?") && body.contains("Not needed any more?"));
        }
    }

    #[test]
    fn the_footnote_counts_its_monitors() {
        let one = render("Uptimepage", &[monitor("a")], None);
        assert!(one.html_body.contains("this monitor or its organization"));
        let two = render("Uptimepage", &[monitor("a"), monitor("b")], None);
        assert!(
            two.html_body
                .contains("these monitors or their organization")
        );
    }

    #[test]
    fn escapes_a_monitor_name_that_carries_markup() {
        let out = render("Uptimepage", &[monitor("<script>alert(1)</script>")], None);
        assert!(!out.html_body.contains("<script>"));
        assert!(out.html_body.contains("&lt;script&gt;"));
    }

    #[test]
    fn a_name_cannot_break_the_subject_line() {
        let out = render("Uptimepage", &[monitor("api\r\nBcc: x@evil.test")], None);
        assert!(!out.subject.contains('\n') && !out.subject.contains('\r'));
    }
}
