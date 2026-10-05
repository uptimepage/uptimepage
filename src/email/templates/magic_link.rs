//! Magic-link sign-in template. Schema + renderer land now so the
//! `EmailTemplate` enum and the `magic_link_tokens` table are wire-compatible
//! the day the request/verify endpoints get wired and
//! `auth.enabled_methods` is extended with `"magic_link"`. No template
//! change required at that point.

use crate::email::templates::layout::{self, ButtonStyle, Page};
use crate::email::templates::{RenderedEmail, html_escape};

pub fn render(
    site_name: &str,
    url: &str,
    code: &str,
    expires_in_minutes: u32,
    ip_hint: Option<&str>,
    opens_accounts: bool,
) -> RenderedEmail {
    // The same mail reaches an address with an account and one without.
    let (subject, action) = if opens_accounts {
        (format!("Your link to {site_name}"), "Continue")
    } else {
        (format!("Sign in to {site_name}"), "Sign in")
    };
    let opening = if opens_accounts {
        format!(
            "Click the link below to continue to {site_name}. It signs you in, or opens an account if you don't have one yet:"
        )
    } else {
        format!("Click the link below to sign in to {site_name}:")
    };
    let ip_line_text = ip_hint
        .map(|ip| format!("\nRequested from {ip}.\n"))
        .unwrap_or_default();

    let text_body = format!(
        "{opening}\n\
         \n  {url}\n\
         \n\
         Or enter this code in the tab you started from:\n\
         \n  {code}\n\
         \n\
         This link expires in {expires_in_minutes} minutes and can only be used once.\n\
         {ip_line_text}\
         If you didn't request this, you can ignore the message.\n"
    );

    let mut body = layout::paragraph(&html_escape(&opening));
    body.push_str(&layout::button(url, action, ButtonStyle::Solid));
    body.push_str(&layout::paragraph(&format!(
        "Or enter <strong>{}</strong> in the tab you started from.",
        html_escape(code)
    )));
    body.push_str(&layout::fine_print(&format!(
        "This link expires in <strong>{expires_in_minutes}</strong> minutes and can only be \
         used once."
    )));
    if let Some(ip) = ip_hint {
        body.push_str(&layout::fine_print(&format!(
            "Requested from {}.",
            html_escape(ip)
        )));
    }

    let html_body = layout::render(Page {
        title: &subject,
        preheader: &format!("One-time link, good for {expires_in_minutes} minutes."),
        signature: Some(site_name),
        header: layout::wordmark(site_name, action),
        body,
        footnote: Some(layout::fine_print(
            "If you didn't request this, you can ignore the message.",
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

    #[test]
    fn an_open_deployment_does_not_promise_an_account_already_exists() {
        let open = render(
            "Uptimepage",
            "https://a.test/v?token=x",
            "4KP9RT",
            15,
            None,
            true,
        );
        assert!(!open.subject.contains("Sign in"), "{}", open.subject);
        // The preheader is the second line an inbox shows unopened.
        assert!(
            !open.html_body.contains("One-time sign-in"),
            "{}",
            open.html_body
        );
        for body in [&open.text_body, &open.html_body] {
            assert!(body.contains("opens an account"), "{body}");
        }

        let closed = render(
            "Uptimepage",
            "https://a.test/v?token=x",
            "4KP9RT",
            15,
            None,
            false,
        );
        assert_eq!(closed.subject, "Sign in to Uptimepage");
        for body in [&closed.text_body, &closed.html_body] {
            assert!(
                !body.contains("opens an account"),
                "invite-only promises nothing of the sort: {body}"
            );
        }
    }

    #[test]
    fn the_code_reaches_both_bodies() {
        let r = render(
            "Uptimepage",
            "https://a.test/v?token=x",
            "4KP9RT",
            15,
            None,
            true,
        );
        assert!(r.text_body.contains("4KP9RT"), "{}", r.text_body);
        assert!(r.html_body.contains("4KP9RT"), "{}", r.html_body);
        assert!(!r.subject.contains("4KP9RT"), "never in the subject line");
    }

    #[test]
    fn the_link_reaches_both_bodies_either_way() {
        for opens in [true, false] {
            let r = render(
                "Uptimepage",
                "https://a.test/v?token=abc",
                "4KP9RT",
                15,
                None,
                opens,
            );
            assert!(r.text_body.contains("https://a.test/v?token=abc"));
            assert!(r.html_body.contains("https://a.test/v?token=abc"));
        }
    }
}
