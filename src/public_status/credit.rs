//! The uptimepage credit a page carries in its footer and in its subscriber
//! mail: whether the plan lets it go, and where it sends a reader.

use crate::domain::Locale;

const SITE: &str = "https://uptimepage.dev";
const LANDING: &str = "/free-status-page";

/// Where a reader met the credit, as analytics names the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Page,
    Email,
}

impl Surface {
    fn source(self) -> &'static str {
        match self {
            Self::Page => "status-page",
            Self::Email => "status-email",
        }
    }

    fn medium(self) -> &'static str {
        match self {
            Self::Page => "powered-by",
            Self::Email => "email",
        }
    }
}

/// White-label is Pro-only: where plans are enforced a plan without white-label
/// always carries the credit, whatever the page stored. Self-host and
/// white-label plans keep the stored preference.
pub fn shown(stored: bool, enforced: bool, white_label: bool) -> bool {
    if enforced && !white_label {
        true
    } else {
        stored
    }
}

/// Subscriber mail carries the credit on hosted pages only: a self-hosted
/// operator's mail is theirs.
pub fn in_mail(stored: bool, enforced: bool, white_label: bool) -> bool {
    enforced && shown(stored, enforced, white_label)
}

/// The free status page landing in the page's language, tagged with the
/// surface and the page's host so a visit can be traced to the page that sent it.
pub fn url(locale: Locale, surface: Surface, host: Option<&str>) -> String {
    let prefix = match locale {
        Locale::En => "",
        Locale::De => "/de",
    };
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query
        .append_pair("utm_source", surface.source())
        .append_pair("utm_medium", surface.medium())
        .append_pair("utm_campaign", "footer");
    if let Some(host) = host {
        query.append_pair("utm_content", host);
    }
    format!("{SITE}{prefix}{LANDING}?{}", query.finish())
}

/// The host, and port if any, of a URL such as `https://acme.uptimepage.dev`.
pub fn host_of(origin: &str) -> Option<String> {
    let url = url::Url::parse(origin).ok()?;
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn powered_by_forced_for_saas_non_white_label() {
        // SaaS free: badge shown even if the page tried to hide it.
        assert!(shown(false, true, false));
        assert!(shown(true, true, false));
        // SaaS white-label (Pro): stored preference honoured.
        assert!(!shown(false, true, true));
        // Self-host: unrestricted, stored preference honoured.
        assert!(!shown(false, false, false));
    }

    #[test]
    fn only_a_hosted_page_puts_the_credit_in_its_mail() {
        assert!(in_mail(false, true, false));
        assert!(!in_mail(false, true, true));
        assert!(in_mail(true, true, true));
        assert!(!in_mail(true, false, false));
    }

    #[test]
    fn the_link_names_its_surface_and_page() {
        assert_eq!(
            url(Locale::En, Surface::Page, Some("acme.uptimepage.dev")),
            "https://uptimepage.dev/free-status-page?utm_source=status-page&utm_medium=powered-by\
             &utm_campaign=footer&utm_content=acme.uptimepage.dev"
        );
        assert_eq!(
            url(Locale::De, Surface::Email, None),
            "https://uptimepage.dev/de/free-status-page?utm_source=status-email&utm_medium=email\
             &utm_campaign=footer"
        );
    }

    #[test]
    fn a_host_cannot_add_parameters_of_its_own() {
        let link = url(Locale::En, Surface::Page, Some("a.test&utm_source=x"));
        assert!(
            link.ends_with("utm_content=a.test%26utm_source%3Dx"),
            "{link}"
        );
    }

    #[test]
    fn host_of_keeps_only_host_and_port() {
        assert_eq!(host_of("https://acme.test").as_deref(), Some("acme.test"));
        assert_eq!(
            host_of("http://acme.test:8080/").as_deref(),
            Some("acme.test:8080")
        );
        assert_eq!(
            host_of("https://ops.acme.test/status-app").as_deref(),
            Some("ops.acme.test")
        );
        assert_eq!(host_of(""), None);
    }
}
