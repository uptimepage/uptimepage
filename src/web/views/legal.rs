//! `/licenses`: the third-party attributions the binary owes, in a minimal
//! layout (no operator nav) that sets no cookies. The operator's own policies
//! live wherever `[legal]` points.

use std::sync::LazyLock;

use askama::Template;
use askama_web::WebTemplate;

use crate::templates::filters;

/// Renders **trusted** markdown to HTML, unsanitised so tables survive. Safe
/// only because the input is first-party Markdown shipped in the repo; never
/// route third-party content through it.
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

static LICENSES: LazyLock<String> =
    LazyLock::new(|| render_trusted_unsanitised(include_str!("../../../THIRD-PARTY-LICENSES.md")));

pub async fn licenses() -> LegalPage {
    LegalPage {
        title: "Third-Party Licenses",
        body: LICENSES.as_str(),
    }
}
