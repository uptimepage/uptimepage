//! Account-restored notification. The deletion mail promised a date; this one
//! retracts it.

use crate::email::templates::RenderedEmail;
use crate::email::templates::layout::{self, Page, Tone};

pub fn render(site_name: &str) -> RenderedEmail {
    let subject = format!("Your {site_name} account has been restored");

    let text_body = format!(
        "The deletion of your {site_name} account has been cancelled.\n\
         \n\
         Your account, your organisations, and their monitors and status pages\n\
         are active again, and monitoring has resumed.\n\
         \n\
         If this was not you, sign in and delete the account again, then get in\n\
         touch — someone else can reach your sign-in method.\n"
    );

    let body = layout::paragraph(
        "Your account, your organisations, and their monitors and status pages are active \
         again, and monitoring has resumed.",
    );

    let html_body = layout::render(Page {
        title: &subject,
        preheader: "The scheduled deletion is cancelled and monitoring has resumed.",
        signature: Some(site_name),
        header: layout::band(
            Tone::Good,
            "ACCOUNT RESTORED",
            "The scheduled deletion is cancelled",
            Some("Monitoring has resumed"),
        ),
        body,
        footnote: Some(layout::fine_print(
            "If this was not you, sign in and delete the account again, then get in touch — \
             someone else can reach your sign-in method.",
        )),
    });

    RenderedEmail {
        subject,
        text_body,
        html_body,
    }
}
