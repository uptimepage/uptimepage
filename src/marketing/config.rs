//! Owned config for the marketing module. A small subset of global
//! config is cloned in at boot — no `AppConfig`, no pool, no `AppState`.

/// Product wordmark. Single source — every Rust-side emitter (OG, JSON-LD,
/// llms.txt, blog renderer) reads from here so a rename is one diff. The
/// askama templates also embed this literal for now; treat the template
/// strings as authored copy that should be re-reviewed on a rename rather
/// than mechanically swapped, but keep this constant authoritative for
/// anything machine-generated.
pub const BRAND: &str = "Uptimepage";

/// Home screenshot gallery, hidden to test whether its absence lifts signups.
/// One switch: the flag gates the section, the tag splits every report the
/// analytics UI can draw, so a traffic swing can't be read as the variant.
pub const GALLERY_VISIBLE: bool = false;
pub const ANALYTICS_TAG: &str = if GALLERY_VISIBLE {
    "gallery-on"
} else {
    "gallery-off"
};

/// Short one-line pitch. Used by llms.txt and as the in-image subtitle
/// on the OG card.
pub const TAGLINE: &str = "Uptime monitoring and public status pages that just work.";

/// `<meta name="description">` + OG `og:description`. Sized to Google's
/// 110–160 char sweet spot so search snippets don't truncate mid-sentence.
pub const META_DESCRIPTION: &str = "Uptime monitoring and public status pages that just work. Eight check types from HTTP and DNS to cron heartbeats and browser logins. Start free, no card.";

/// Automation surfaces on their own prod hosts — not derived from
/// `canonical_origin` (separate hostnames), so authored absolute.
pub const MCP_URL: &str = "https://mcp.uptimepage.dev/mcp";
/// The official MCP registry has no per-server permalink; its catalogue links
/// outward, so a search query is the only stable public proof of the listing.
pub const MCP_REGISTRY_URL: &str =
    "https://registry.modelcontextprotocol.io/v0.1/servers?search=dev.uptimepage/uptimepage";
pub const TERRAFORM_URL: &str = "https://registry.terraform.io/providers/uptimepage/uptimepage";
pub const SOURCE_URL: &str = "https://github.com/uptimepage/uptimepage";

/// Must match the GitHub org profile and the about page, or the brand resolves
/// to more than one entity.
pub const CONTACT_EMAIL: &str = "hello@uptimepage.dev";
pub const ORG_LOCALITY: &str = "Nicosia";
pub const ORG_COUNTRY: &str = "CY";
/// Month the first commit landed.
pub const ORG_FOUNDING_DATE: &str = "2026-05";

/// Named blog author — a verifiable Person for search-engine E-E-A-T.
#[derive(Debug, Clone)]
pub struct Author {
    pub name: &'static str,
    pub role: &'static str,
    pub bio: &'static str,
    pub url: &'static str,
    pub image: &'static str,
    pub same_as: &'static [(&'static str, &'static str)],
}

pub const AUTHOR: Author = Author {
    name: "Artem Senenko",
    role: "Founder & Software Engineer, Uptimepage",
    bio: "Software engineer with 20+ years building and running production \
          systems: microservice architecture on Kubernetes, cloud \
          infrastructure on AWS and Terraform, and security-critical SaaS in \
          the fintech domain.",
    url: "https://www.linkedin.com/in/artem-senenko-b3195927/",
    image: "img/authors/artem-senenko.jpg",
    same_as: &[
        (
            "LinkedIn",
            "https://www.linkedin.com/in/artem-senenko-b3195927/",
        ),
        ("GitHub", "https://github.com/slima4"),
        ("X", "https://x.com/sl1ma4"),
        ("Mastodon", "https://mastodon.social/@slima4"),
        ("Strivle", "https://www.strivle.com/u/slima4"),
    ],
};

/// What the marketing handlers read: the relevant fields of
/// `crate::config::MarketingConfig`, plus what the discovery surfaces need
/// from other config sections. Owns its fields so this struct compiles
/// untouched in the extracted service.
#[derive(Debug, Clone)]
pub struct MarketingCfg {
    pub app_url: String,
    pub canonical_origin: String,
    pub blog_enabled: bool,
    /// This deployment's MCP endpoint. Never falls back to [`MCP_URL`]:
    /// the catalog is a machine contract, that constant is hosted-only copy.
    pub mcp_url: Option<String>,
    /// The app sells the paid plans: a payment provider is configured and at
    /// least one plan is priced, so the pricing page may send a visitor to
    /// checkout. Off, the pro and team cards say "soon". Read once at boot,
    /// like the page it gates.
    pub checkout_open: bool,
    /// Reverse proxies whose `X-Forwarded-For` may be believed. Empty means
    /// the TCP peer is the client. Read by the tools that open an outbound
    /// socket, to key their per-IP budget on the visitor rather than on Caddy.
    pub trusted_proxies: Vec<ipnet::IpNet>,
}
