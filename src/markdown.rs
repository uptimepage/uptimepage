//! Customer-written Markdown: incident and maintenance text, and a status
//! page's About. The parser output is never trusted: the ammonia allow-list
//! is the boundary, not pulldown-cmark.

use std::sync::LazyLock;

use ammonia::{Builder, UrlRelative};
use pulldown_cmark::{Event, LinkType, Parser, Tag, TagEnd, html};

const ABOUT_TAGS: [&str; 8] = ["p", "strong", "em", "a", "br", "ul", "ol", "li"];
const TEXT_TAGS: [&str; 10] = [
    "p", "strong", "em", "a", "br", "ul", "ol", "li", "code", "pre",
];

static ABOUT: LazyLock<Builder<'static>> = LazyLock::new(|| allow_list(&ABOUT_TAGS));
static WEB: LazyLock<Builder<'static>> = LazyLock::new(sanitizer);

fn allow_list(tags: &[&'static str]) -> Builder<'static> {
    let mut b = Builder::default();
    b.tags(tags.iter().copied().collect())
        .link_rel(Some("noopener nofollow"));
    b
}

/// The allow-list for incident and maintenance text; mail adds inline styles
/// to it. A relative link loses its target: the same text shows on the page,
/// in the console and in mail, which share no base to resolve it against.
pub(crate) fn sanitizer() -> Builder<'static> {
    let mut b = allow_list(&TEXT_TAGS);
    b.url_relative(UrlRelative::Deny);
    b
}

/// A status page's About, as plain CommonMark.
pub(crate) fn about_html(text: &str) -> String {
    clean(Parser::new(text), &ABOUT)
}

/// Incident or maintenance text for a page or the console.
pub(crate) fn to_html(text: &str) -> String {
    render(text, &WEB)
}

/// Incident or maintenance text through `builder`. Every line break stays a
/// break, as it was when this text was plain, and raw HTML shows as typed: a
/// block of it becomes paragraphs of its lines. What the allow-list would
/// strip keeps its words: a heading reads as a paragraph, an image as its alt
/// text, and a link with no web or mail target as its text.
pub(crate) fn render(text: &str, builder: &Builder<'_>) -> String {
    let mut events = Vec::new();
    let mut raw_block = String::new();
    let mut dropped_link = false;
    for event in Parser::new(text) {
        match event {
            Event::SoftBreak => events.push(Event::HardBreak),
            Event::InlineHtml(raw) => {
                for (i, line) in raw.split('\n').enumerate() {
                    if i > 0 {
                        events.push(Event::HardBreak);
                    }
                    events.push(Event::Text(line.trim_end_matches('\r').to_owned().into()));
                }
            }
            Event::Html(raw) => raw_block.push_str(&raw),
            Event::End(TagEnd::HtmlBlock) => {
                raw_paragraphs(&raw_block, &mut events);
                raw_block.clear();
            }
            Event::Start(Tag::Heading { .. }) => events.push(Event::Start(Tag::Paragraph)),
            Event::End(TagEnd::Heading(_)) => events.push(Event::End(TagEnd::Paragraph)),
            Event::Start(Tag::Link {
                link_type,
                ref dest_url,
                ..
            }) if link_type != LinkType::Email && !has_target(dest_url) => dropped_link = true,
            Event::End(TagEnd::Link) if dropped_link => dropped_link = false,
            Event::Start(Tag::HtmlBlock | Tag::Image { .. }) | Event::End(TagEnd::Image) => {}
            other => events.push(other),
        }
    }
    clean(events.into_iter(), builder)
}

fn has_target(url: &str) -> bool {
    ["http://", "https://", "mailto:"].iter().any(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
    })
}

/// A raw HTML block as text: a break between its lines, a new paragraph after
/// a blank one.
fn raw_paragraphs(block: &str, events: &mut Vec<Event<'_>>) {
    let mut open = false;
    let mut gap = false;
    for line in block.lines() {
        if line.trim().is_empty() {
            gap = open;
            continue;
        }
        if !open {
            events.push(Event::Start(Tag::Paragraph));
            open = true;
        } else if gap {
            events.push(Event::End(TagEnd::Paragraph));
            events.push(Event::Start(Tag::Paragraph));
        } else {
            events.push(Event::HardBreak);
        }
        gap = false;
        events.push(Event::Text(line.to_owned().into()));
    }
    if open {
        events.push(Event::End(TagEnd::Paragraph));
    }
}

/// Whether the text carries no Markdown, so [`to_plain`] reads as the page
/// shows it.
pub(crate) fn is_plain(text: &str) -> bool {
    Parser::new(text).all(|event| {
        matches!(
            event,
            Event::Text(_)
                | Event::SoftBreak
                | Event::HardBreak
                | Event::InlineHtml(_)
                | Event::Html(_)
                | Event::Rule
                | Event::Start(Tag::Paragraph | Tag::HtmlBlock)
                | Event::End(TagEnd::Paragraph | TagEnd::HtmlBlock)
        )
    })
}

fn clean<'a>(events: impl Iterator<Item = Event<'a>>, builder: &Builder<'_>) -> String {
    let mut html = String::new();
    html::push_html(&mut html, events);
    builder.clean(&html).to_string()
}

/// The words without the markup, for previews and one-line places. A list
/// item keeps its bullet or number, a block its blank line after it.
pub(crate) fn to_plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut gap = 0;
    for event in Parser::new(text) {
        match event {
            Event::Text(t) | Event::Code(t) | Event::Html(t) | Event::InlineHtml(t) => {
                put(&mut out, &mut gap, &t);
            }
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            Event::Start(Tag::List(start)) => lists.push(start),
            Event::Start(Tag::Item) => {
                gap = gap.max(1);
                let marker = match lists.last_mut() {
                    Some(Some(n)) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    _ => "• ".to_owned(),
                };
                put(&mut out, &mut gap, &marker);
            }
            Event::End(TagEnd::List(_)) => {
                lists.pop();
                gap = if lists.is_empty() { 2 } else { 1 };
            }
            Event::End(
                TagEnd::Paragraph
                | TagEnd::Heading(_)
                | TagEnd::BlockQuote(_)
                | TagEnd::CodeBlock
                | TagEnd::HtmlBlock,
            ) => gap = if lists.is_empty() { 2 } else { 1 },
            _ => {}
        }
    }
    out.truncate(out.trim_end().len());
    out
}

fn put(out: &mut String, gap: &mut usize, piece: &str) {
    if *gap > 0 && !out.is_empty() {
        out.truncate(out.trim_end_matches('\n').len());
        out.extend(std::iter::repeat_n('\n', *gap));
    }
    *gap = 0;
    out.push_str(piece);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_allow_list_and_drops_the_rest() {
        let html =
            to_html("**bold** _em_ `code`\n\n- a\n- b\n\n1. one\n\n![img](https://x.test/a.png)");
        assert!(html.contains("<strong>bold</strong>"), "{html}");
        assert!(html.contains("<em>em</em>"), "{html}");
        assert!(html.contains("<code>code</code>"), "{html}");
        assert!(html.contains("<li>a</li>"), "{html}");
        assert!(html.contains("<ol>"), "{html}");
        assert!(!html.contains("<img"), "{html}");
    }

    #[test]
    fn single_line_breaks_stay_breaks() {
        assert_eq!(to_html("one\ntwo"), "<p>one<br>\ntwo</p>\n");
    }

    #[test]
    fn raw_html_shows_as_typed() {
        let html = to_html("<script>alert(1)</script>\n\nrun <b>this</b> on <hostname>");
        assert!(!html.contains("<script"), "{html}");
        assert!(!html.contains("<b>"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{html}"
        );
        assert!(
            html.contains("run &lt;b&gt;this&lt;/b&gt; on &lt;hostname&gt;"),
            "{html}"
        );
    }

    #[test]
    fn a_block_of_raw_html_keeps_its_lines() {
        assert_eq!(
            to_html("<api-gateway>\nrestarted at 10:00\nmonitoring now"),
            "<p>&lt;api-gateway&gt;<br>\nrestarted at 10:00<br>\nmonitoring now</p>\n"
        );
        let pasted = to_html("<html>\r\n<body>\r\n502 Bad Gateway");
        assert!(
            pasted.contains("&lt;html&gt;<br>\n&lt;body&gt;<br>\n502 Bad Gateway"),
            "{pasted}"
        );
        assert_eq!(
            to_html("<pre>\nGET /a 502\n\nGET /b 502\n</pre>"),
            "<p>&lt;pre&gt;<br>\nGET /a 502</p>\n<p>GET /b 502<br>\n&lt;/pre&gt;</p>\n"
        );
    }

    #[test]
    fn a_link_with_no_web_or_mail_target_reads_as_its_text() {
        assert_eq!(
            to_html("see [the incident](/status/incidents/42) or [x](javascript:alert(1))"),
            "<p>see the incident or x</p>\n"
        );
        assert!(to_html("<ops@acme.test>").contains("href=\"mailto:ops@acme.test\""));
        assert!(about_html("[status](/status)").contains("href=\"/status\""));
    }

    #[test]
    fn what_the_allow_list_strips_keeps_its_words() {
        let html =
            to_html("# Phase 1\n\n![latency graph](https://x.test/g.png)\n\n---\n\n> quoted");
        for kept in ["<p>Phase 1</p>", "<p>latency graph</p>", "<p>quoted</p>"] {
            assert!(html.contains(kept), "{html}");
        }
        assert!(!html.contains("<img") && !html.contains("---"), "{html}");
        assert_eq!(
            to_html("error from <upstream\nservice> at 10:02"),
            "<p>error from &lt;upstream<br>\nservice&gt; at 10:02</p>\n"
        );
    }

    #[test]
    fn plain_text_is_told_from_markdown() {
        assert!(is_plain("Done early.\r\nNo data lost.\n\nAT&T <hostname>"));
        assert!(is_plain("<html>\n502 Bad Gateway"));
        assert!(!is_plain("Done **early**."));
        assert!(!is_plain("- one\n- two"));
        assert!(!is_plain("see [notes](https://x.test)"));
    }

    #[test]
    fn links_get_rel_and_lose_script_urls() {
        let html = to_html("[docs](https://example.com) [bad](javascript:alert(1))");
        assert!(
            html.contains("<a href=\"https://example.com\" rel=\"noopener nofollow\">docs</a>"),
            "{html}"
        );
        assert!(!html.contains("javascript"), "{html}");
    }

    #[test]
    fn indented_text_keeps_its_lines() {
        let html = to_html("Log:\n\n    line one\n    line two");
        assert!(
            html.contains("<pre><code>line one\nline two</code></pre>"),
            "{html}"
        );
    }

    #[test]
    fn about_keeps_plain_commonmark() {
        assert_eq!(about_html("one\ntwo"), "<p>one\ntwo</p>\n");
        assert!(!about_html("`code`").contains("<code>"));
        assert!(!about_html("<script>alert(1)</script>").contains("alert"));
    }

    #[test]
    fn plain_text_drops_the_markup() {
        assert_eq!(
            to_plain("We **restarted** the [database](https://x.test).\nBack soon."),
            "We restarted the database.\nBack soon."
        );
    }

    #[test]
    fn plain_text_keeps_blocks_and_list_markers() {
        assert_eq!(
            to_plain("Steps:\n\n- drain\n- restart\n\n3. three\n4. four\n\nDone."),
            "Steps:\n\n• drain\n• restart\n\n3. three\n4. four\n\nDone."
        );
    }

    #[test]
    fn plain_text_is_unchanged_for_plain_input() {
        let text = "Line one\nLine two\n\nAT&T <hostname> 5 * 3";
        assert_eq!(to_plain(text), text);
    }
}
