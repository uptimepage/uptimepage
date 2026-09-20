//! Counts assistant crawlers fetching the marketing surface.
//!
//! The browser tracker already reports assistant *referrals* — someone read
//! an answer and clicked through. It cannot see the fetch that produced the
//! answer, because crawlers run no JavaScript. This middleware closes that
//! half: it is the only place a `GPTBot` or `ClaudeBot` hit is observable.
//!
//! `kind` is the label that carries meaning. A `user-fetch` agent is dispatched
//! because a person asked a question seconds ago, so it tracks live citation. A
//! `crawler` is building a corpus for answers weeks away. Averaging the two
//! hides the signal worth watching.
//!
//! Every label comes from a fixed table — unknown agents are not counted at
//! all — so a sprayed `User-Agent` cannot open new series. Same cardinality
//! contract as `http_metrics`.

use axum::extract::Request;
use axum::http::StatusCode;
use axum::http::header::USER_AGENT;
use axum::middleware::Next;
use axum::response::Response;
use metrics::counter;

use crate::metric_names;

/// Why the agent came. Determined by which agent it is, not by the request.
const CRAWLER: &str = "crawler";
const USER_FETCH: &str = "user-fetch";

/// Matched case-insensitively as substrings of `User-Agent`, first hit wins.
/// A needle that is a prefix of a later one would shadow it, so the longer
/// token is always listed first — `needles_are_not_shadowed` enforces that
/// rather than leaving it to whoever adds the next vendor.
const AGENTS: &[(&str, &str, &str)] = &[
    // (needle, bot label, kind)
    ("chatgpt-user", "chatgpt-user", USER_FETCH),
    ("claude-user", "claude-user", USER_FETCH),
    ("perplexity-user", "perplexity-user", USER_FETCH),
    ("mistralai-user", "mistralai-user", USER_FETCH),
    ("duckassistbot", "duckassistbot", USER_FETCH),
    ("oai-searchbot", "oai-searchbot", CRAWLER),
    ("gptbot", "gptbot", CRAWLER),
    ("claude-searchbot", "claude-searchbot", CRAWLER),
    ("claudebot", "claudebot", CRAWLER),
    ("anthropic-ai", "anthropic-ai", CRAWLER),
    ("perplexitybot", "perplexitybot", CRAWLER),
    ("ccbot", "ccbot", CRAWLER),
    ("google-cloudvertexbot", "google-cloudvertexbot", CRAWLER),
    ("meta-externalagent", "meta-externalagent", CRAWLER),
    ("bytespider", "bytespider", CRAWLER),
    ("amazonbot", "amazonbot", CRAWLER),
    // Apple's AI-training agent, kept ahead of the search crawler whose name
    // it contains — collapsing them would hide which one of the two came.
    ("applebot-extended", "applebot-extended", CRAWLER),
    ("applebot", "applebot", CRAWLER),
    ("cohere-ai", "cohere-ai", CRAWLER),
    ("youbot", "youbot", CRAWLER),
    ("timpibot", "timpibot", CRAWLER),
    ("diffbot", "diffbot", CRAWLER),
];

/// Which body of content was fetched. Coarse on purpose: the question is
/// which *kind* of page assistants read, and a per-URL label would put the
/// whole sitemap into the metric.
fn section(path: &str) -> Option<&'static str> {
    // Asset fetches would outnumber page fetches and say nothing about what
    // an assistant read.
    if path.starts_with("/static") {
        return None;
    }
    let section = if path == "/" {
        "home"
    } else if path.starts_with("/blog") {
        "blog"
    } else if path.starts_with("/docs") {
        "docs"
    } else if path.starts_with("/compare/") {
        "compare"
    } else if path.starts_with("/vs/") {
        "vs"
    } else if path.starts_with("/tools") {
        "tools"
    } else if is_index_path(path) {
        "index"
    } else {
        "landing"
    };
    Some(section)
}

/// The files an agent reads to discover the rest, kept in one bucket so a
/// bot doing discovery is distinguishable from one reading content.
fn is_index_path(path: &str) -> bool {
    matches!(
        path,
        "/robots.txt" | "/sitemap.xml" | "/llms.txt" | "/llms-full.txt"
    ) || path.starts_with("/.well-known")
}

/// Whether the response handed the agent the page, either as a body or as a
/// "you already have it". Anything else means there was nothing to read.
fn is_read(status: StatusCode) -> bool {
    status.is_success() || status == StatusCode::NOT_MODIFIED
}

/// First matching agent wins; anything unrecognised is left uncounted.
fn classify(user_agent: &str) -> Option<(&'static str, &'static str)> {
    let ua = user_agent.to_ascii_lowercase();
    AGENTS
        .iter()
        .find(|(needle, _, _)| ua.contains(needle))
        .map(|(_, bot, kind)| (*bot, *kind))
}

pub async fn middleware(req: Request, next: Next) -> Response {
    let counted = req
        .headers()
        .get(USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .and_then(classify)
        .zip(section(req.uri().path()));

    let response = next.run(req).await;

    // Counted after the handler, and only when the page was there to read. A
    // 404 from the marketing fallback is a stale link rather than something an
    // assistant read, and `section` would still label it from the dead path.
    // 304 counts: every marketing route is ETagged, so a recrawling bot
    // revalidates rather than refetching, and dropping those would show
    // assistant traffic collapsing while the crawlers were still coming.
    if is_read(response.status())
        && let Some(((bot, kind), section)) = counted
    {
        counter!(
            metric_names::AI_CRAWLER_REQUESTS,
            "bot" => bot,
            "kind" => kind,
            "section" => section,
        )
        .increment(1);
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_live_citation_fetches_from_corpus_crawls() {
        assert_eq!(
            classify(
                "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko); compatible; ChatGPT-User/1.0; +https://openai.com/bot"
            ),
            Some(("chatgpt-user", USER_FETCH))
        );
        assert_eq!(
            classify("Mozilla/5.0 (compatible; GPTBot/1.2; +https://openai.com/gptbot)"),
            Some(("gptbot", CRAWLER))
        );
        assert_eq!(
            classify("Mozilla/5.0 (compatible; ClaudeBot/1.0; +claudebot@anthropic.com)"),
            Some(("claudebot", CRAWLER))
        );
        assert_eq!(
            classify("Mozilla/5.0 (compatible; Claude-User/1.0; +Claude-User@anthropic.com)"),
            Some(("claude-user", USER_FETCH))
        );
        assert_eq!(
            classify("Mozilla/5.0 (compatible; PerplexityBot/1.0)"),
            Some(("perplexitybot", CRAWLER))
        );
        assert_eq!(
            classify("Perplexity-User/1.0"),
            Some(("perplexity-user", USER_FETCH))
        );
    }

    #[test]
    fn needles_are_not_shadowed() {
        for (i, (earlier, label, _)) in AGENTS.iter().enumerate() {
            for (later, later_label, _) in &AGENTS[i + 1..] {
                assert!(
                    !later.contains(earlier),
                    "`{later_label}` is unreachable: every UA carrying `{later}` \
                     also carries `{earlier}`, which `{label}` claims first"
                );
            }
        }
    }

    #[test]
    fn apple_training_agent_is_distinct_from_its_search_crawler() {
        assert_eq!(
            classify("Mozilla/5.0 (compatible; Applebot-Extended/0.1)"),
            Some(("applebot-extended", CRAWLER))
        );
        assert_eq!(
            classify("Mozilla/5.0 (compatible; Applebot/0.1)"),
            Some(("applebot", CRAWLER))
        );
    }

    #[test]
    fn a_revalidated_recrawl_still_counts() {
        // Every marketing route is ETagged, so a returning crawler gets 304.
        // Dropping those would show assistant traffic falling to nothing while
        // the bots kept arriving.
        assert!(is_read(StatusCode::OK));
        assert!(is_read(StatusCode::NOT_MODIFIED));
        for nothing_to_read in [
            StatusCode::NOT_FOUND,
            StatusCode::MOVED_PERMANENTLY,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
        ] {
            assert!(!is_read(nothing_to_read), "{nothing_to_read}");
        }
    }

    #[test]
    fn matching_ignores_case() {
        assert_eq!(classify("gptbot/1.0"), Some(("gptbot", CRAWLER)));
        assert_eq!(classify("GPTBOT/1.0"), Some(("gptbot", CRAWLER)));
    }

    #[test]
    fn ordinary_and_hostile_agents_open_no_series() {
        for ua in [
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Chrome/140.0",
            "Googlebot/2.1 (+http://www.google.com/bot.html)",
            "curl/8.7.1",
            "",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert_eq!(classify(ua), None, "{ua}");
        }
    }

    #[test]
    fn sections_stay_within_the_fixed_set() {
        assert_eq!(section("/"), Some("home"));
        assert_eq!(section("/blog"), Some("blog"));
        assert_eq!(section("/blog/uptime-sla"), Some("blog"));
        assert_eq!(section("/docs/api"), Some("docs"));
        assert_eq!(section("/compare/uptime-kuma-vs-gatus"), Some("compare"));
        assert_eq!(section("/vs/pingdom"), Some("vs"));
        assert_eq!(section("/tools/uptime-sla-calculator"), Some("tools"));
        assert_eq!(section("/open-source-uptime-monitoring"), Some("landing"));
    }

    #[test]
    fn discovery_files_are_one_bucket() {
        for path in [
            "/robots.txt",
            "/sitemap.xml",
            "/llms.txt",
            "/llms-full.txt",
            "/.well-known/mcp.json",
        ] {
            assert_eq!(section(path), Some("index"), "{path}");
        }
    }

    #[test]
    fn asset_fetches_are_not_counted() {
        assert_eq!(section("/static/css/app.css"), None);
        assert_eq!(section("/static/img/org-icon.png"), None);
    }
}
