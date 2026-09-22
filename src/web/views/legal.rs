//! Public legal & policy pages: `/terms`, `/privacy`, `/cookies`,
//! `/impressum`, `/abuse-policy`, `/security-policy`. The RFC 9116
//! `/.well-known/security.txt` is mounted alongside them by the router,
//! from `security::disclosure` — it is plain text, shared with the
//! marketing host, and renders through none of this.
//!
//! The markdown is first-party content compiled into the binary with
//! `include_str!` and rendered to HTML once on first request. It is
//! *trusted* author content — unlike user-supplied `public_about`, it is
//! deliberately not run through ammonia, so headings and tables survive.
//!
//! These pages render in their own minimal layout (no operator nav): they
//! are reached from the footer by signed-out visitors as often as by
//! operators, and they set no cookies.

use std::sync::LazyLock;

use askama::Template;
use askama_web::WebTemplate;

use crate::templates::filters;

/// Renders **trusted** markdown to HTML. This path is deliberately
/// **unsanitised** — tables and the occasional raw `<a>` in the Privacy
/// Policy must survive — and is safe **only** because every input is
/// first-party Markdown shipped in the repo. The unusual symbol name
/// is the visible mistake: anyone reusing this for third-party PR
/// content (e.g. a blog post body) inherits the trust model and ships
/// an XSS hole. Use the marketing blog's `render` (which goes through
/// `ammonia::clean`) for any content that did not originate in this
/// repo.
fn render_trusted_unsanitised(markdown: &str) -> String {
    let mut opts = pulldown_cmark::Options::empty();
    opts.insert(pulldown_cmark::Options::ENABLE_TABLES);
    let parser = pulldown_cmark::Parser::new_ext(markdown, opts);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    html
}

#[derive(Template, WebTemplate)]
#[template(path = "legal.html")]
pub struct LegalPage {
    title: &'static str,
    /// Pre-rendered, trusted HTML. The only `|safe` value on the page.
    body: &'static str,
}

/// Binds one markdown file to a route handler. The rendered HTML is built
/// once (`LazyLock`) and borrowed for the program's lifetime.
macro_rules! legal_page {
    ($html:ident, $handler:ident, $title:literal, $file:literal) => {
        static $html: LazyLock<String> =
            LazyLock::new(|| render_trusted_unsanitised(include_str!($file)));

        pub async fn $handler() -> LegalPage {
            LegalPage {
                title: $title,
                body: $html.as_str(),
            }
        }
    };
}

legal_page!(
    TERMS,
    terms,
    "Terms of Service",
    "../../../docs/legal/terms.md"
);
legal_page!(
    PRIVACY,
    privacy,
    "Privacy Policy",
    "../../../docs/legal/privacy.md"
);
legal_page!(
    COOKIES,
    cookies,
    "Cookie Policy",
    "../../../docs/legal/cookies.md"
);
legal_page!(
    IMPRESSUM,
    impressum,
    "Impressum",
    "../../../docs/legal/impressum.md"
);
legal_page!(
    ABUSE,
    abuse_policy,
    "Abuse Policy",
    "../../../docs/legal/abuse-policy.md"
);
legal_page!(
    SECURITY,
    security_policy,
    "Security Policy",
    "../../../docs/legal/security-policy.md"
);
// AGPL-3.0 / Apache-2.0 / MIT / 0BSD redistribution: the binary embeds
// third-party assets, so their attributions travel with it and are served
// here. Reuses the same trusted-markdown path as the policy pages.
legal_page!(
    LICENSES,
    licenses,
    "Third-Party Licenses",
    "../../../THIRD-PARTY-LICENSES.md"
);
