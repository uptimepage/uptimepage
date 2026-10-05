use chrono::{DateTime, Utc};

use crate::email::templates::layout::{self, ButtonStyle, Page};
use crate::email::templates::{RenderedEmail, html_escape, utc_stamp};

pub fn render(
    site_name: &str,
    org_name: &str,
    inviter_display: &str,
    accept_url: &str,
    decline_url: &str,
    expires_at: DateTime<Utc>,
) -> RenderedEmail {
    let expires_human = utc_stamp(expires_at);
    // Subject and preheader are what an inbox shows unopened; the org and the
    // sender are named by whoever sent this.
    let subject = format!("You have an invitation on {site_name}");
    let preheader = "Accept or decline it from this message.";

    let text_body = format!(
        "{inviter_display} invited you to join {org_name} on {site_name}.\n\
         \n\
         Accept the invitation:\n  {accept_url}\n\
         \n\
         Or decline:\n  {decline_url}\n\
         \n\
         This link expires at {expires_human}.\n\
         \n\
         If you weren't expecting this, you can ignore the message — no\n\
         account is created and the invitation will be cleaned up after it\n\
         expires.\n"
    );

    let mut body = layout::paragraph(&format!(
        "{inviter} invited you to join <strong>{org}</strong> on {site}.",
        inviter = html_escape(inviter_display),
        org = html_escape(org_name),
        site = html_escape(site_name),
    ));
    body.push_str(&layout::button(
        accept_url,
        "Accept invitation",
        ButtonStyle::Solid,
    ));
    body.push_str(&layout::fine_print(&format!(
        "Or {decline}. This link expires at <strong>{expires}</strong>.",
        decline = layout::link(decline_url, "decline"),
        expires = html_escape(&expires_human),
    )));

    let html_body = layout::render(Page {
        title: &subject,
        preheader,
        signature: Some(site_name),
        header: layout::wordmark(site_name, "You're invited"),
        body,
        footnote: Some(layout::fine_print(
            "If you weren't expecting this, you can ignore the message — no account is created.",
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
    use super::render;
    use chrono::{TimeZone, Utc};

    fn hostile() -> crate::email::templates::RenderedEmail {
        render(
            "Uptimepage",
            "verify your account, action required",
            "Uptimepage Security",
            "https://app.test/accept",
            "https://app.test/decline",
            Utc.with_ymd_and_hms(2026, 8, 30, 9, 0, 0).unwrap(),
        )
    }

    #[test]
    fn nothing_the_sender_chose_reaches_the_inbox_list() {
        let r = hostile();
        assert_eq!(r.subject, "You have an invitation on Uptimepage");
        let preheader = r
            .html_body
            .split("Accept or decline it from this message.")
            .count();
        assert_eq!(preheader, 2, "the preheader is ours: {}", r.html_body);
        assert!(!r.subject.contains("action required"));
    }

    #[test]
    fn who_is_asking_still_reaches_the_body() {
        let r = hostile();
        for body in [&r.text_body, &r.html_body] {
            assert!(body.contains("Uptimepage Security"), "inviter: {body}");
            assert!(body.contains("action required"), "org: {body}");
            assert!(body.contains("https://app.test/accept"), "accept: {body}");
        }
    }

    #[test]
    fn a_name_carrying_markup_is_escaped_into_the_html_body() {
        let r = render(
            "Uptimepage",
            "<script>x</script>",
            "<img src=x onerror=y>",
            "https://app.test/accept",
            "https://app.test/decline",
            Utc.with_ymd_and_hms(2026, 8, 30, 9, 0, 0).unwrap(),
        );
        assert!(!r.html_body.contains("<script>x"), "{}", r.html_body);
        assert!(!r.html_body.contains("<img src=x"), "{}", r.html_body);
        assert!(r.html_body.contains("&lt;script&gt;"));
    }
}
