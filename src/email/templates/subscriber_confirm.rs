use crate::email::templates::layout::{self, ButtonStyle, Page};
use crate::email::templates::{MARKUP_SLOT, fill_slot, html_escape};
use crate::email::trait_def::RenderedEmail;
use crate::i18n::Tr;

pub fn render(
    tr: Tr,
    site_name: &str,
    page_name: &str,
    confirm_url: &str,
    expires_hours: u32,
    unsubscribe_url: &str,
) -> RenderedEmail {
    let subject = tr.t_args("email-confirm-subject", [("page", page_name.into())]);
    let expiry = tr.t_args("email-confirm-expiry", [("hours", expires_hours.into())]);

    let text_body = format!(
        "{intro}\n\
         \n\
         {cta}\n\
         \n  {confirm_url}\n\
         \n\
         {expiry}\n\
         {not_you}\n\
         \n  {unsubscribe_url}\n",
        intro = tr.t_args(
            "email-confirm-intro",
            [("page", page_name.into()), ("site", site_name.into())],
        ),
        cta = tr.t("email-confirm-cta"),
        expiry = expiry,
        not_you = tr.t("email-confirm-not-you"),
    );

    let mut body = layout::paragraph(&fill_slot(
        &tr.t_args("email-confirm-lead", [("page", MARKUP_SLOT.into())]),
        &format!("<strong>{}</strong>", html_escape(page_name)),
    ));
    body.push_str(&layout::button(
        confirm_url,
        &tr.t("email-confirm-button"),
        ButtonStyle::Solid,
    ));
    body.push_str(&layout::fine_print(&html_escape(&expiry)));

    let footnote = layout::fine_print(&fill_slot(
        &tr.t_args("email-confirm-footnote", [("remove", MARKUP_SLOT.into())]),
        &layout::quiet_link(unsubscribe_url, &tr.t("email-confirm-remove")),
    ));

    let html_body = layout::render_in(
        tr.lang(),
        Page {
            title: &subject,
            preheader: &tr.t("email-confirm-preheader"),
            signature: Some(site_name),
            header: layout::wordmark(site_name, &tr.t("email-confirm-heading")),
            body,
            footnote: Some(footnote),
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
    fn both_the_confirm_and_the_opt_out_survive_rendering() {
        let r = render(
            Tr::default(),
            "Uptimepage",
            "Acme status",
            "https://acme.test/subscribe/confirm?token=x",
            24,
            "https://acme.test/subscribe/unsubscribe?s=1&t=2",
        );
        assert!(r.subject.contains("Acme status"));
        assert!(r.html_body.contains("subscribe/confirm?token=x"));
        assert!(r.html_body.contains("Remove this address"));
        assert!(r.text_body.contains("subscribe/unsubscribe?s=1&t=2"));
    }

    #[test]
    fn a_german_page_confirms_in_german() {
        let r = render(
            Tr::new(crate::domain::Locale::De),
            "Uptimepage",
            "Acme & Co",
            "https://acme.test/subscribe/confirm?token=x",
            24,
            "https://acme.test/subscribe/unsubscribe?s=1&t=2",
        );
        assert_eq!(r.subject, "Bestätigen Sie Ihr Abonnement für Acme & Co");
        assert!(r.html_body.contains(r#"<html lang="de">"#));
        assert!(r.html_body.contains("<strong>Acme &amp; Co</strong>"));
        assert!(r.html_body.contains("Dieser Link ist 24 Stunden gültig"));
        assert!(
            r.html_body
                .contains(">Diese Adresse entfernen</a>, dann senden")
        );
        assert!(r.text_body.contains("Dieser Link ist 24 Stunden gültig"));
        for body in [&r.subject, &r.text_body, &r.html_body] {
            assert!(
                !body.contains(['\u{2068}', '\u{2069}', '\u{E000}']),
                "{body}"
            );
        }
    }
}
