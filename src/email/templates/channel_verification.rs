use crate::email::templates::layout::{self, ButtonStyle, Page};
use crate::email::templates::{RenderedEmail, html_escape};

pub fn render(
    site_name: &str,
    channel_name: &str,
    verify_url: &str,
    expires_hours: u32,
    org_name: Option<&str>,
    decline_url: Option<&str>,
) -> RenderedEmail {
    let subject = format!("Verify this address for {site_name} alerts");
    let by_org = org_name.map(|o| format!(" by {o}")).unwrap_or_default();
    let decline_text = decline_url
        .map(|u| format!(", or block this address: {u}"))
        .unwrap_or_default();

    let text_body = format!(
        "This address was added as the alert channel \"{channel_name}\"{by_org} on {site_name}.\n\
         \n\
         Confirm it to start receiving alerts:\n\
         \n  {verify_url}\n\
         \n\
         This link expires in {expires_hours} hours and can only be used once.\n\
         If you didn't expect this, ignore this message{decline_text} — no alerts \
         are sent until it is confirmed.\n"
    );

    let by_org_html = org_name
        .map(|o| format!(" by <strong>{}</strong>", html_escape(o)))
        .unwrap_or_default();

    let mut body = layout::paragraph(&format!(
        "This address was added as the alert channel <strong>{channel}</strong>{by_org_html}. \
         Confirm it to start receiving alerts.",
        channel = html_escape(channel_name),
    ));
    body.push_str(&layout::button(
        verify_url,
        "Verify address",
        ButtonStyle::Solid,
    ));
    body.push_str(&layout::fine_print(&format!(
        "This link expires in <strong>{expires_hours}</strong> hours and can only be used once."
    )));

    let mut footnote = "No alerts are sent until this address is confirmed.".to_string();
    if let Some(url) = decline_url {
        footnote.push_str(" Didn't expect this? ");
        footnote.push_str(&layout::quiet_link(url, "Block this address"));
        footnote.push('.');
    }

    let html_body = layout::render(Page {
        title: &subject,
        preheader: "Confirm this address before any alert is delivered to it.",
        signature: Some(site_name),
        header: layout::wordmark(site_name, "Verify this address for alerts"),
        body,
        footnote: Some(layout::fine_print(&footnote)),
    });

    RenderedEmail {
        subject,
        text_body,
        html_body,
    }
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn org_attribution_and_decline_link_appear() {
        let decline = "https://app/alert-channel/stop?c=1&t=2";
        let r = render(
            "Uptimepage",
            "Ops",
            "https://app/verify?token=t",
            24,
            Some("Acme Inc"),
            Some(decline),
        );
        assert!(r.text_body.contains("by Acme Inc"));
        assert!(r.text_body.contains(decline));
        assert!(r.html_body.contains("Acme Inc"));
        assert!(r.html_body.contains("Block this address"));
    }

    #[test]
    fn renders_without_attribution() {
        let r = render(
            "Uptimepage",
            "Ops",
            "https://app/verify?token=t",
            24,
            None,
            None,
        );
        assert!(r.text_body.contains("alert channel \"Ops\" on Uptimepage"));
        assert!(!r.text_body.contains(" by "));
        assert!(!r.html_body.contains("Block this address"));
    }
}
