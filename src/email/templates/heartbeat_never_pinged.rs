use crate::email::templates::layout::{self, ButtonStyle, Page};
use crate::email::templates::{RenderedEmail, duration_words, html_escape};

pub struct UnwiredHeartbeat<'a> {
    pub monitor_name: &'a str,
    pub waiting_secs: i64,
    /// Deep link to the monitor. The ping URL itself never rides the mail: it
    /// is a write capability that cannot be rotated, so it stays behind a login.
    pub monitor_url: Option<&'a str>,
    pub docs_url: Option<&'a str>,
    pub org_name: Option<&'a str>,
}

pub fn render(site_name: &str, h: &UnwiredHeartbeat<'_>) -> RenderedEmail {
    let subject = format!("\"{}\" has never been pinged", h.monitor_name);
    let for_org = h.org_name.map(|o| format!(" in {o}")).unwrap_or_default();
    let waited = duration_words(h.waiting_secs);

    let mut text_body = format!(
        "The heartbeat monitor \"{name}\"{for_org} was created {waited} ago and has not \
         received a single ping.\n\
         \n\
         Nothing is broken. A heartbeat is not watched until the job it belongs to \
         reports in for the first time, so this monitor is not alerting anyone and \
         will not until then. If you meant to finish wiring it up, the ping URL is on \
         the monitor's page.\n",
        name = h.monitor_name,
    );
    if let Some(url) = h.monitor_url {
        text_body.push_str(&format!("\nOpen the monitor:\n\n  {url}\n"));
    }
    if let Some(url) = h.docs_url {
        text_body.push_str(&format!("\nHow heartbeats work:\n\n  {url}\n"));
    }
    text_body.push_str("\nThis is the only reminder we send about it.\n");

    let mut body = layout::paragraph(&format!(
        "The heartbeat monitor <strong>{name}</strong>{for_org_html} was created \
         <strong>{waited}</strong> ago and has not received a single ping.",
        name = html_escape(h.monitor_name),
        for_org_html = h
            .org_name
            .map(|o| format!(" in <strong>{}</strong>", html_escape(o)))
            .unwrap_or_default(),
    ));
    body.push_str(&layout::paragraph(
        "Nothing is broken. A heartbeat is not watched until the job it belongs to \
         reports in for the first time, so this monitor is not alerting anyone and \
         will not until then. If you meant to finish wiring it up, the ping URL is \
         on the monitor's page.",
    ));
    if let Some(url) = h.monitor_url {
        body.push_str(&layout::button(url, "Open the monitor", ButtonStyle::Solid));
    }
    if let Some(url) = h.docs_url {
        body.push_str(&layout::paragraph(&layout::quiet_link(
            url,
            "How heartbeats work",
        )));
    }

    let html_body = layout::render(Page {
        title: &subject,
        preheader: "This heartbeat has never been pinged, so nothing is watching it yet.",
        signature: Some(site_name),
        header: layout::wordmark(site_name, "A heartbeat is still waiting to be wired up"),
        body,
        footnote: Some(layout::fine_print(
            "You are receiving this because you own this monitor. \
             This is the only reminder sent about it.",
        )),
    });

    RenderedEmail {
        subject,
        text_body,
        html_body,
    }
}

#[cfg(test)]
mod tests {
    use super::{UnwiredHeartbeat, render};

    fn sample() -> UnwiredHeartbeat<'static> {
        UnwiredHeartbeat {
            monitor_name: "nightly backup",
            waiting_secs: 3 * 86_400 + 2 * 3_600,
            monitor_url: Some("https://app.test/targets/abc"),
            docs_url: Some("https://uptimepage.com/docs/monitor-types"),
            org_name: Some("Acme Inc"),
        }
    }

    #[test]
    fn says_it_is_unwatched_without_claiming_an_outage() {
        let out = render("Uptimepage", &sample());
        assert_eq!(out.subject, "\"nightly backup\" has never been pinged");
        for body in [&out.text_body, &out.html_body] {
            assert!(body.contains("Nothing is broken"));
            assert!(body.contains("not alerting anyone"));
            assert!(
                !body.to_lowercase().contains("down"),
                "a monitor that never ran is not down"
            );
        }
        assert!(out.text_body.contains("3d 2h"));
    }

    /// The ping URL is an unrotatable write capability, so the mail links to
    /// the page holding it rather than shipping a copy to an inbox.
    #[test]
    fn never_carries_the_ping_url_itself() {
        let out = render("Uptimepage", &sample());
        for body in [&out.text_body, &out.html_body] {
            assert!(!body.contains("/ping/"));
        }
    }

    #[test]
    fn escapes_a_monitor_name_that_carries_markup() {
        let out = render(
            "Uptimepage",
            &UnwiredHeartbeat {
                monitor_name: "<script>alert(1)</script>",
                ..sample()
            },
        );
        assert!(!out.html_body.contains("<script>"));
        assert!(out.html_body.contains("&lt;script&gt;"));
    }

    #[test]
    fn omits_the_links_it_was_not_given() {
        let out = render(
            "Uptimepage",
            &UnwiredHeartbeat {
                monitor_url: None,
                docs_url: None,
                org_name: None,
                ..sample()
            },
        );
        assert!(!out.text_body.contains("Open the monitor"));
        assert!(!out.html_body.contains("How heartbeats work"));
    }
}
