//! The five subscription mails, to the account owner. Every one of them
//! says the same thing about data: nothing is deleted, the excess is held.

use chrono::{DateTime, Utc};

use crate::domain::Landing;
use crate::email::templates::layout::{self, ButtonStyle, Page, Tone};
use crate::email::templates::{html_escape, utc_stamp};
use crate::email::trait_def::RenderedEmail;

/// Rows a smaller plan does not cover. Zero on both means the plan fits.
#[derive(Debug, Clone, Copy)]
pub struct Excess {
    pub monitors: i64,
    pub pages: i64,
}

impl Excess {
    fn is_empty(self) -> bool {
        self.monitors <= 0 && self.pages <= 0
    }

    /// "3 monitors and 1 status page", or an empty string when nothing is over.
    fn words(self) -> String {
        let mut parts = Vec::new();
        if self.monitors > 0 {
            parts.push(plural(self.monitors, "monitor"));
        }
        if self.pages > 0 {
            parts.push(plural(self.pages, "status page"));
        }
        parts.join(" and ")
    }
}

fn plural(n: i64, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

const HELD_NOT_DELETED: &str = "Held means kept: the rows stay exactly as they are and stop running. \
     Paying again releases every one of them.";

pub fn payment_failed(
    site_name: &str,
    plan_name: &str,
    retry_by: DateTime<Utc>,
    fix_url: &str,
) -> RenderedEmail {
    let deadline = utc_stamp(retry_by);
    let subject = format!("Payment failed for your {site_name} {plan_name} plan");

    let text_body = format!(
        "We could not collect the payment for your {plan_name} plan.\n\
         \n\
         Nothing changes yet: every monitor, check and status page keeps\n\
         running until {deadline}. Update the card before then and the\n\
         plan continues as if nothing happened.\n\
         \n\
         Update your payment method: {fix_url}\n\
         \n\
         If the payment is still missing on {deadline}, the account moves to\n\
         the plan it had before paying. Anything above that plan's limits is\n\
         held, not deleted, and comes back the moment a payment goes through.\n"
    );

    let mut body = layout::facts(&[
        ("Plan", plan_name.to_string()),
        ("Full service until", deadline.clone()),
    ]);
    body.push_str(&layout::paragraph(
        "Nothing changes yet: every monitor, check and status page keeps running until then. \
         Update the card before the deadline and the plan continues as if nothing happened.",
    ));
    body.push_str(&layout::button(
        fix_url,
        "Update payment method",
        ButtonStyle::Solid,
    ));
    body.push_str(&layout::fine_print(&format!(
        "If the payment is still missing on {}, the account moves to the plan it had before \
         paying. Anything above that plan's limits is held, not deleted, and comes back the \
         moment a payment goes through.",
        html_escape(&deadline)
    )));

    let html_body = layout::render(Page {
        title: &subject,
        preheader: &format!("Full service continues until {deadline}."),
        signature: Some(site_name),
        header: layout::band(
            Tone::Warn,
            "PAYMENT FAILED",
            &format!("{plan_name} plan continues until {deadline}"),
            Some("Update the card to keep it"),
        ),
        body,
        footnote: None,
    });

    RenderedEmail {
        subject,
        text_body,
        html_body,
    }
}

pub fn payment_recovered(site_name: &str, plan_name: &str) -> RenderedEmail {
    let subject = format!("Payment received for your {site_name} {plan_name} plan");

    let text_body = format!(
        "The payment for your {plan_name} plan went through. The plan\n\
         continues unchanged, and anything that was held has been released.\n"
    );

    let body = layout::paragraph(
        "The plan continues unchanged, and anything that was held has been released.",
    );

    let html_body = layout::render(Page {
        title: &subject,
        preheader: "The plan continues unchanged.",
        signature: Some(site_name),
        header: layout::band(
            Tone::Good,
            "PAYMENT RECEIVED",
            &format!("{plan_name} plan continues"),
            None,
        ),
        body,
        footnote: None,
    });

    RenderedEmail {
        subject,
        text_body,
        html_body,
    }
}

pub fn downgrade_scheduled(
    site_name: &str,
    plan_name: &str,
    at: DateTime<Utc>,
    over: Excess,
    keep_url: &str,
) -> RenderedEmail {
    let when = utc_stamp(at);
    let subject = format!("Your {site_name} plan moves to {plan_name} on {when}");

    let excess_text = if over.is_empty() {
        "Everything you have fits the new plan, so nothing is held.".to_string()
    } else {
        format!(
            "The new plan does not cover {}. Unless you choose, the newest ones are held \
             from that date. Choose what to keep: {keep_url}",
            over.words()
        )
    };
    let text_body = format!(
        "Your plan changes to {plan_name} on {when}. Until then nothing changes;\n\
         you keep what you paid for.\n\
         \n\
         {excess_text}\n\
         \n\
         {HELD_NOT_DELETED}\n"
    );

    let mut body = layout::facts(&[
        ("New plan", plan_name.to_string()),
        ("Takes effect", when.clone()),
    ]);
    body.push_str(&layout::paragraph(
        "Until then nothing changes; you keep what you paid for.",
    ));
    if over.is_empty() {
        body.push_str(&layout::paragraph(
            "Everything you have fits the new plan, so nothing is held.",
        ));
    } else {
        body.push_str(&layout::callout(&format!(
            "The new plan does not cover {}. Unless you choose, the newest ones are held from \
             that date.",
            over.words()
        )));
        body.push_str(&layout::button(
            keep_url,
            "Choose what to keep",
            ButtonStyle::Solid,
        ));
    }
    body.push_str(&layout::fine_print(HELD_NOT_DELETED));

    let html_body = layout::render(Page {
        title: &subject,
        preheader: &format!("Nothing changes until {when}."),
        signature: Some(site_name),
        header: layout::band(
            Tone::Info,
            "PLAN CHANGE BOOKED",
            &format!("{plan_name} from {when}"),
            None,
        ),
        body,
        footnote: None,
    });

    RenderedEmail {
        subject,
        text_body,
        html_body,
    }
}

pub fn downgrade_applied(
    site_name: &str,
    plan_name: &str,
    landing: Landing,
    held: Excess,
    keep_url: &str,
) -> RenderedEmail {
    let subject = format!("Your {site_name} account is now on the {plan_name} plan");

    let why = match landing {
        Landing::Scheduled => "Your subscription has moved to this plan.",
        Landing::Canceled => {
            "Your subscription was cancelled, so the account is back on the plan it had \
             before paying."
        }
        Landing::Paused => {
            "Your subscription is paused at the payment provider, so the account is back on \
             the plan it had before paying until it resumes."
        }
        Landing::Unpaid => {
            "The payment for your previous plan was not received before the deadline, so the \
             account has moved to the plan it had before paying."
        }
    };
    let held_text = if held.is_empty() {
        "Everything you have fits this plan, so nothing is held.".to_string()
    } else {
        format!(
            "Now held: {}. Choose different ones to keep: {keep_url}",
            held.words()
        )
    };
    let text_body = format!("{why}\n\n{held_text}\n\n{HELD_NOT_DELETED}\n");

    let mut body = layout::facts(&[("Plan", plan_name.to_string())]);
    body.push_str(&layout::paragraph(why));
    if held.is_empty() {
        body.push_str(&layout::paragraph(
            "Everything you have fits this plan, so nothing is held.",
        ));
    } else {
        body.push_str(&layout::callout(&format!("Now held: {}.", held.words())));
        body.push_str(&layout::button(
            keep_url,
            "Choose what to keep",
            ButtonStyle::Outline,
        ));
    }
    body.push_str(&layout::fine_print(HELD_NOT_DELETED));

    let html_body = layout::render(Page {
        title: &subject,
        preheader: why,
        signature: Some(site_name),
        header: layout::band(
            if landing == Landing::Unpaid {
                Tone::Warn
            } else {
                Tone::Info
            },
            "PLAN CHANGED",
            &format!("Now on {plan_name}"),
            None,
        ),
        body,
        footnote: None,
    });

    RenderedEmail {
        subject,
        text_body,
        html_body,
    }
}

pub fn subscription_canceled(
    site_name: &str,
    plan_name: &str,
    ends_at: DateTime<Utc>,
) -> RenderedEmail {
    let when = utc_stamp(ends_at);
    let subject = format!("Your {site_name} subscription ends on {when}");

    let text_body = format!(
        "Your subscription is cancelled and paid service ends on {when}. Until\n\
         then nothing changes; you keep what you paid for.\n\
         \n\
         From that date the account is on the {plan_name} plan. Anything above\n\
         its limits is held, not deleted, and comes back if you subscribe again.\n"
    );

    let mut body = layout::facts(&[
        ("Paid service ends", when.clone()),
        ("Plan after that", plan_name.to_string()),
    ]);
    body.push_str(&layout::paragraph(
        "Until then nothing changes; you keep what you paid for.",
    ));
    body.push_str(&layout::fine_print(
        "Anything above the new plan's limits is held, not deleted, and comes back if you \
         subscribe again.",
    ));

    let html_body = layout::render(Page {
        title: &subject,
        preheader: &format!("Paid service ends on {when}."),
        signature: Some(site_name),
        header: layout::band(
            Tone::Info,
            "SUBSCRIPTION CANCELLED",
            &format!("Paid service ends {when}"),
            None,
        ),
        body,
        footnote: None,
    });

    RenderedEmail {
        subject,
        text_body,
        html_body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 1, 9, 0, 0).unwrap()
    }

    #[test]
    fn payment_failed_carries_the_deadline_and_the_fix_link() {
        let r = payment_failed(
            "Uptimepage",
            "Team",
            at(),
            "https://app/settings/billing/payment-method",
        );
        for body in [&r.text_body, &r.html_body] {
            assert!(body.contains("1 Oct 2026 09:00 UTC"), "{body}");
            assert!(body.contains("settings/billing/payment-method"), "{body}");
        }
        assert!(r.subject.contains("Team"));
    }

    #[test]
    fn a_downgrade_that_fits_says_nothing_is_held() {
        let r = downgrade_scheduled(
            "Uptimepage",
            "Pro",
            at(),
            Excess {
                monitors: 0,
                pages: 0,
            },
            "https://app/settings/usage",
        );
        assert!(r.text_body.contains("nothing is held"));
        assert!(!r.html_body.contains("Choose what to keep"));
    }

    #[test]
    fn a_downgrade_over_cap_counts_the_excess_and_links_the_picker() {
        let r = downgrade_scheduled(
            "Uptimepage",
            "Pro",
            at(),
            Excess {
                monitors: 3,
                pages: 1,
            },
            "https://app/settings/usage",
        );
        assert!(r.text_body.contains("3 monitors and 1 status page"));
        assert!(r.html_body.contains("Choose what to keep"));
        assert!(r.html_body.contains("https://app/settings/usage"));
    }

    #[test]
    fn each_landing_explains_itself() {
        let held = Excess {
            monitors: 1,
            pages: 0,
        };
        let text = |landing| {
            downgrade_applied(
                "Uptimepage",
                "Founding",
                landing,
                held,
                "https://app/settings/usage",
            )
            .text_body
        };
        let unpaid = text(Landing::Unpaid);
        assert!(unpaid.contains("not received before the deadline"));
        assert!(unpaid.contains("Now held: 1 monitor."));
        assert!(text(Landing::Canceled).contains("was cancelled"));
        assert!(text(Landing::Paused).contains("is paused"));
        assert!(text(Landing::Scheduled).contains("has moved to this plan"));
    }
}
