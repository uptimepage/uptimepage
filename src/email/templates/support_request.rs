use crate::email::templates::layout::{self, Page};
use crate::email::templates::{RenderedEmail, single_line};

/// What the operator needs to answer without a round-trip. Only `message` and
/// `page_url` come from the caller; the rest is server-derived.
pub struct SupportContext<'a> {
    pub request_id: &'a str,
    pub topic: &'a str,
    pub message: &'a str,
    pub from_email: &'a str,
    pub org_slug: &'a str,
    pub org_id: &'a str,
    pub plan: &'a str,
    pub page_url: Option<&'a str>,
    pub app_version: &'a str,
}

/// The final group, not a prefix: under UUIDv7 the leading groups are the
/// timestamp, so requests in the same window would share them.
pub fn short_ref(request_id: &str) -> &str {
    request_id.rsplit('-').next().unwrap_or(request_id)
}

pub fn render(site_name: &str, ctx: &SupportContext<'_>) -> RenderedEmail {
    // ASCII-only: one non-ASCII byte forces the subject into an RFC 2047
    // encoded word, hiding the topic and reference from filters and grep.
    let subject = format!(
        "[help/{} #{}] {} - {}",
        single_line(ctx.topic),
        short_ref(ctx.request_id),
        single_line(ctx.org_slug),
        single_line(ctx.from_email),
    );

    let mut rows: Vec<(&str, String)> = vec![
        (
            "Ref",
            format!("{} ({})", short_ref(ctx.request_id), ctx.request_id),
        ),
        ("From", ctx.from_email.to_string()),
        ("Org", format!("{} ({})", ctx.org_slug, ctx.org_id)),
        ("Plan", ctx.plan.to_string()),
        ("Topic", ctx.topic.to_string()),
        ("Version", ctx.app_version.to_string()),
    ];
    if let Some(url) = ctx.page_url {
        rows.push(("Page", url.to_string()));
    }

    let facts_text = rows
        .iter()
        .map(|(label, value)| format!("{label}: {value}"))
        .collect::<Vec<_>>()
        .join("\n");

    let text_body = format!(
        "{facts_text}\n\n{}\n\nReply to this mail to answer them directly.\n",
        ctx.message
    );

    let mut body = layout::facts(&rows);
    body.push_str(&layout::code_block(ctx.message));

    let html_body = layout::render(Page {
        title: &subject,
        preheader: &single_line(ctx.message),
        signature: Some(site_name),
        header: layout::wordmark(
            site_name,
            &format!(
                "help / {topic} · {org}",
                topic = single_line(ctx.topic),
                org = single_line(ctx.org_slug),
            ),
        ),
        body,
        footnote: Some(layout::fine_print(
            "Reply to this mail to answer them directly.",
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
    use super::{SupportContext, render, short_ref};

    const REQUEST_ID: &str = "0198f0e1-1111-7222-8333-a1b2c3d4e5f6";

    fn ctx<'a>(topic: &'a str, message: &'a str) -> SupportContext<'a> {
        SupportContext {
            request_id: REQUEST_ID,
            topic,
            message,
            from_email: "jane@acme.test",
            org_slug: "acme",
            org_id: "0198f0e1-0000-7000-8000-000000000000",
            plan: "founding",
            page_url: Some("/targets/42"),
            app_version: "1.0.0",
        }
    }

    #[test]
    fn subject_carries_topic_reference_org_and_sender() {
        let r = render("Uptimepage", &ctx("bug", "checks are flapping"));
        assert_eq!(
            r.subject, "[help/bug #a1b2c3d4e5f6] acme - jane@acme.test",
            "prefix sorts by topic, the reference is quotable"
        );
    }

    #[test]
    fn subject_stays_ascii_so_filters_and_grep_see_it_unencoded() {
        // A non-ASCII byte forces RFC 2047 encoded-word wrapping, after which
        // neither a mail filter nor grep matches the topic or the reference.
        let r = render("Uptimepage", &ctx("bug", "body"));
        assert!(r.subject.is_ascii(), "subject: {}", r.subject);
    }

    #[test]
    fn the_short_reference_is_the_random_tail_not_the_timestamp() {
        // Two UUIDv7s minted in the same millisecond share every leading
        // group; only the final group tells them apart.
        assert_eq!(short_ref(REQUEST_ID), "a1b2c3d4e5f6");
        assert_eq!(
            short_ref("0198f0e1-1111-7222-8333-ffffffffffff"),
            "ffffffffffff"
        );
        assert_eq!(short_ref("no-dashes"), "dashes", "degrades, never panics");
        assert_eq!(short_ref(""), "");
    }

    #[test]
    fn both_the_short_and_canonical_reference_reach_the_body() {
        let r = render("Uptimepage", &ctx("bug", "body"));
        for body in [&r.text_body, &r.html_body] {
            assert!(body.contains("a1b2c3d4e5f6"), "quotable form");
            assert!(body.contains(REQUEST_ID), "exact-lookup form");
        }
    }

    #[test]
    fn subject_never_spans_multiple_lines() {
        let r = render("Uptimepage", &ctx("bug\r\nBcc: evil@example.test", "body"));
        assert!(!r.subject.contains('\n'));
        assert!(!r.subject.contains('\r'));
        assert!(
            r.subject.contains("Bcc: evil@example.test"),
            "text kept, folding removed"
        );
    }

    #[test]
    fn context_and_message_reach_both_bodies() {
        let r = render("Uptimepage", &ctx("question", "how do I add a region?"));
        for body in [&r.text_body, &r.html_body] {
            assert!(body.contains("acme"));
            assert!(body.contains("founding"));
            assert!(body.contains("/targets/42"));
            assert!(body.contains("how do I add a region?"));
        }
    }

    #[test]
    fn message_markup_is_escaped_in_html() {
        let r = render("Uptimepage", &ctx("bug", "<img src=x onerror=alert(1)>"));
        assert!(!r.html_body.contains("<img src=x"));
        assert!(r.html_body.contains("&lt;img src=x"));
    }
}
