//! Account-deletion notification email. Sent once, right after a successful
//! `DELETE /api/v1/me`. Tells the user the account is deactivated and when it
//! is permanently erased. No link to carry: restoring runs through a
//! signed-in confirmation, not this mail.

use chrono::{DateTime, Utc};

use crate::email::templates::layout::{self, Page, Tone};
use crate::email::templates::{RenderedEmail, utc_stamp};

pub fn render(site_name: &str, scheduled_purge_at: DateTime<Utc>) -> RenderedEmail {
    let purge_human = utc_stamp(scheduled_purge_at);
    let subject = format!("Your {site_name} account is scheduled for deletion");

    let text_body = format!(
        "Your {site_name} account has been deactivated and is scheduled for\n\
         permanent deletion on {purge_human}. Monitoring has stopped: your\n\
         checks are no longer running and no alerts will be sent.\n\
         \n\
         Changed your mind? Sign in before {purge_human} and confirm the\n\
         restore on the page that appears; signing in on its own will not\n\
         cancel the deletion.\n\
         \n\
         After that the account and all associated data are permanently erased\n\
         and cannot be recovered.\n\
         \n\
         If you requested this deletion, no further action is needed.\n"
    );

    let mut body = layout::facts(&[("Erased on", purge_human.clone())]);
    body.push_str(&layout::paragraph(
        "Monitoring has stopped: your checks are no longer running and no alerts will be sent.",
    ));
    body.push_str(&layout::callout(&format!(
        "Changed your mind? Sign in before {purge_human} and confirm the restore on the page \
         that appears — signing in on its own will not cancel the deletion."
    )));
    body.push_str(&layout::fine_print(
        "After that the account and all associated data are permanently erased and cannot be \
         recovered.",
    ));

    let html_body = layout::render(Page {
        title: &subject,
        preheader: &format!("All data is erased on {purge_human} unless you restore it."),
        signature: Some(site_name),
        header: layout::band(
            Tone::Warn,
            "ACCOUNT DEACTIVATED",
            &format!("Permanently erased on {purge_human}"),
            Some(&format!("{site_name} monitoring has stopped")),
        ),
        body,
        footnote: Some(layout::fine_print(
            "If you requested this deletion, no further action is needed.",
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

    #[test]
    fn the_purge_date_reaches_both_bodies() {
        let r = render(
            "Uptimepage",
            Utc.with_ymd_and_hms(2026, 9, 1, 8, 30, 0).unwrap(),
        );
        for body in [&r.text_body, &r.html_body] {
            assert!(body.contains("1 Sep 2026 08:30 UTC"), "purge date: {body}");
        }
    }
}
