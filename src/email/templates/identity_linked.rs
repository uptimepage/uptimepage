//! Sign-in method added. Linking turns a provider account into a credential
//! for this one, so it never happens quietly.

use crate::email::templates::layout::{self, ButtonStyle, Page, Tone};
use crate::email::templates::{RenderedEmail, html_escape, single_line};

pub fn render(site_name: &str, provider_label: &str, account_url: &str) -> RenderedEmail {
    let provider = single_line(provider_label);
    let subject = format!("{provider} was added to your {site_name} account");

    let text_body = format!(
        "{provider} can now sign in to your {site_name} account.\n\
         \n\
         If you added it, nothing to do.\n\
         \n\
         If you did not, remove it and sign out everywhere:\n\
         {account_url}\n"
    );

    let body = layout::paragraph(&format!(
        "{} can now sign in to your {} account. If you added it, there is nothing to do.",
        html_escape(&provider),
        html_escape(site_name),
    )) + &layout::button(account_url, "Review sign-in methods", ButtonStyle::Outline);

    let html_body = layout::render(Page {
        title: &subject,
        preheader: "A new sign-in method can now open your account.",
        signature: Some(site_name),
        header: layout::band(
            Tone::Info,
            "SIGN-IN METHOD ADDED",
            &format!("{provider} was added"),
            Some("Review it if this was not you"),
        ),
        body,
        footnote: Some(layout::fine_print(
            "If you did not add this, remove it from your account and sign out everywhere.",
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
    fn the_provider_and_a_way_to_review_reach_both_bodies() {
        let r = render("Uptimepage", "GitLab", "https://app.test/settings/account");
        assert!(r.subject.contains("GitLab"));
        for body in [&r.text_body, &r.html_body] {
            assert!(body.contains("GitLab"), "provider: {body}");
            assert!(
                body.contains("https://app.test/settings/account"),
                "the mail is useless without somewhere to act: {body}"
            );
        }
    }

    #[test]
    fn a_provider_label_is_escaped_into_the_html_body() {
        // Our own enum today; this guards the day it is not.
        let r = render("Uptimepage", "<script>x</script>", "https://app.test/a");
        assert!(!r.html_body.contains("<script>x"), "{}", r.html_body);
        assert!(r.html_body.contains("&lt;script&gt;"));
    }
}
