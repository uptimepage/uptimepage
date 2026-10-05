use crate::email::templates::layout::{self, ButtonStyle, Page};
use crate::email::templates::{RenderedEmail, duration_words, html_escape};

pub struct FailingChannel<'a> {
    pub channel_name: &'a str,
    pub transport: &'a str,
    pub org_name: Option<&'a str>,
    pub consecutive_failures: i32,
    /// How long the run of failures has been going.
    pub failing_secs: Option<i64>,
    /// Last error the transport returned, redacted upstream.
    pub last_error: Option<&'a str>,
    /// Deep link to the channel's settings page; omitted when no base URL.
    pub channel_url: Option<&'a str>,
}

pub fn render(site_name: &str, c: &FailingChannel<'_>) -> RenderedEmail {
    let subject = format!("Alerts are not reaching \"{}\"", c.channel_name);
    let for_org = c.org_name.map(|o| format!(" in {o}")).unwrap_or_default();
    let duration = c
        .failing_secs
        .map(|secs| format!(" for {}", duration_words(secs)))
        .unwrap_or_default();
    // "its last alert" reads as one event; "its last 3 alerts" as a run.
    let count = match c.consecutive_failures {
        1 => "alert".to_string(),
        n => format!("{n} alerts"),
    };

    // The channel is still being paged, so the reader is being told about a
    // gap they can still close, not one already closed for them.
    let mut text_body = format!(
        "The {transport} channel \"{name}\"{for_org} has failed to deliver its last \
         {count}{duration}.\n\
         \n\
         Alerts are still being sent to it, so nothing is turned off. But if this \
         channel is where your outage alerts go, they are not arriving.\n",
        transport = c.transport,
        name = c.channel_name,
    );
    if let Some(err) = c.last_error {
        text_body.push_str(&format!("\nWhat the endpoint returned:\n\n  {err}\n"));
    }
    if let Some(url) = c.channel_url {
        text_body.push_str(&format!("\nCheck the channel:\n\n  {url}\n"));
    }

    let mut body = layout::paragraph(&format!(
        "The {transport} channel <strong>{name}</strong>{for_org_html} has failed to \
         deliver its last <strong>{count}</strong>{duration}.",
        transport = html_escape(c.transport),
        name = html_escape(c.channel_name),
        for_org_html = c
            .org_name
            .map(|o| format!(" in <strong>{}</strong>", html_escape(o)))
            .unwrap_or_default(),
    ));
    body.push_str(&layout::paragraph(
        "Alerts are still being sent to it, so nothing is turned off. But if this \
         channel is where your outage alerts go, they are not arriving.",
    ));
    if let Some(err) = c.last_error {
        body.push_str(&layout::paragraph(&format!(
            "What the endpoint returned: <code>{}</code>",
            html_escape(err)
        )));
    }
    if let Some(url) = c.channel_url {
        body.push_str(&layout::button(
            url,
            "Check the channel",
            ButtonStyle::Solid,
        ));
    }

    let html_body = layout::render(Page {
        title: &subject,
        preheader: "This channel has stopped delivering. Alerts sent to it are not arriving.",
        signature: Some(site_name),
        header: layout::wordmark(site_name, "A notification channel is not delivering"),
        body,
        footnote: Some(layout::fine_print(
            "You are receiving this because you own this account. \
             One message is sent per outage of a channel, not per alert.",
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
    use super::{FailingChannel, duration_words, render};

    fn sample() -> FailingChannel<'static> {
        FailingChannel {
            channel_name: "on-call",
            transport: "webhook",
            org_name: Some("Acme Inc"),
            consecutive_failures: 3,
            failing_secs: Some(2 * 86_400 + 3 * 3_600),
            last_error: Some("endpoint returned 404 Not Found"),
            channel_url: Some("https://app.test/settings/notifications/abc/edit"),
        }
    }

    #[test]
    fn names_the_channel_the_run_and_the_error() {
        let r = render("Uptimepage", &sample());
        assert!(r.subject.contains("on-call"));
        for body in [&r.text_body, &r.html_body] {
            assert!(body.contains("on-call"));
            assert!(body.contains("Acme Inc"));
            assert!(body.contains(&duration_words(2 * 86_400 + 3 * 3_600)));
            assert!(body.contains("404 Not Found"));
        }
        assert!(r.html_body.contains("settings/notifications/abc/edit"));
    }

    /// The mail has to say the channel is still live, or a reader assumes we
    /// turned it off for them and stops looking.
    #[test]
    fn says_alerts_are_still_being_sent() {
        let r = render("Uptimepage", &sample());
        assert!(r.text_body.contains("still being sent"));
        assert!(r.html_body.contains("still being sent"));
    }

    /// A `channel_failure_limit` of one still has to read as English.
    #[test]
    fn a_single_failure_reads_singular() {
        let r = render(
            "Uptimepage",
            &FailingChannel {
                consecutive_failures: 1,
                ..sample()
            },
        );
        assert!(r.text_body.contains("its last alert"));
        assert!(!r.text_body.contains("1 alerts"));
        assert!(r.html_body.contains("<strong>alert</strong>"));
        assert!(!r.html_body.contains("1 alerts"));
    }

    #[test]
    fn optional_context_is_omitted_not_blank() {
        let r = render(
            "Uptimepage",
            &FailingChannel {
                org_name: None,
                failing_secs: None,
                last_error: None,
                channel_url: None,
                ..sample()
            },
        );
        assert!(!r.text_body.contains(" in "));
        assert!(!r.text_body.contains(" for "));
        assert!(!r.html_body.contains("Check the channel"));
    }
}
