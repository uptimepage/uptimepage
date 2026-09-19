//! Anti-abuse admission control: a compiled URL-pattern deny-list, a domain
//! deny-list (YAML), and an optional hosts-format reputation feed, checked
//! at target-creation time so a blocked target never enters the scheduler
//! in the first place.
//!
//! Built once at startup from [`AbuseConfig`]. [`AbuseGuard::validate`] is the
//! strict gate `main` runs at config-validation time: a malformed regex or
//! YAML file is a clean startup error there, never a panic in construction
//! (the enforcement invariant on config-derived values). The runtime
//! [`AbuseGuard::from_config`] is total — it degrades (logs + skips) rather
//! than failing, because by the time it runs `validate` has already proven
//! the inputs good.

use std::collections::HashSet;
use std::sync::Arc;

use arc_swap::ArcSwap;
use regex::RegexSet;
use serde::Deserialize;

use crate::config::AbuseConfig;
use crate::domain::CheckSpec;
use crate::error::codes;
use crate::error::{AppError, Result};

/// What an abuse match was. Drives the audit `quota_name`, the API error
/// code, and the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbuseKind {
    UrlPattern,
    Domain,
    Reputation,
}

/// A positive abuse match: the rule that fired and the offending detail
/// (the matched pattern, or the deny-listed domain/parent).
#[derive(Debug, Clone)]
pub struct AbuseHit {
    pub kind: AbuseKind,
    pub detail: String,
}

impl AbuseHit {
    /// `quota_events.quota_name` value for this block.
    pub fn quota_name(&self) -> &'static str {
        match self.kind {
            AbuseKind::UrlPattern => "url_pattern",
            AbuseKind::Domain => "domain_denylist",
            AbuseKind::Reputation => "reputation",
        }
    }

    /// 400 error carrying the stable code. URL-pattern and reputation
    /// matches surface as the generic `ABUSE_BLOCKED`; static deny-list
    /// matches as `DOMAIN_DENYLISTED`.
    pub fn into_app_error(self) -> AppError {
        let (code, msg) = match self.kind {
            AbuseKind::UrlPattern => (
                codes::ABUSE_BLOCKED,
                "target URL matches a blocked abuse pattern".to_string(),
            ),
            AbuseKind::Domain => (
                codes::DOMAIN_DENYLISTED,
                format!("target domain '{}' is on the deny-list", self.detail),
            ),
            AbuseKind::Reputation => (
                codes::ABUSE_BLOCKED,
                format!("target domain '{}' has a poor reputation", self.detail),
            ),
        };
        AppError::bad_request_field(code, msg, "check.url")
    }
}

/// YAML shape of `config/abuse_denylist.yaml`. Only `domain` is consumed;
/// `category` / `reported_at` are operator-facing metadata and tolerated.
#[derive(Debug, Deserialize)]
struct DenylistFile {
    #[serde(default)]
    domains: Vec<DenylistEntry>,
}

#[derive(Debug, Deserialize)]
struct DenylistEntry {
    domain: String,
}

/// The hot-swappable rule set. [`AbuseGuard::reload`] replaces it
/// atomically; readers in [`AbuseGuard::inspect`] take a cheap `ArcSwap`
/// snapshot, so a reload never blocks a scheduler check and an in-flight
/// check always sees one consistent set (never a half-updated mix).
struct AbuseState {
    url_patterns: RegexSet,
    domain_denylist: HashSet<String>,
    /// Hosts ingested from the optional reputation feed. Same parent-domain
    /// match as the deny-list; separate set so a hit reports its own kind.
    reputation: HashSet<String>,
}

pub struct AbuseGuard {
    state: ArcSwap<AbuseState>,
}

impl AbuseGuard {
    /// Strict config-validation gate. Compiles every URL pattern and parses
    /// the deny-list file, returning a field-named error on the first
    /// problem. `main` calls this so a bad pattern / YAML is a clean startup
    /// config error rather than a silent degrade or a boot panic.
    pub fn validate(cfg: &AbuseConfig) -> Result<()> {
        for p in &cfg.url_patterns_denied {
            regex::Regex::new(p).map_err(|e| {
                AppError::Other(anyhow::anyhow!(
                    "abuse.url_patterns_denied: invalid regex {p:?}: {e}"
                ))
            })?;
        }
        if let Some(raw) = read_optional_file(&cfg.domain_denylist_path)? {
            serde_norway::from_str::<DenylistFile>(&raw).map_err(|e| {
                AppError::Other(anyhow::anyhow!(
                    "abuse.domain_denylist_path ({}): invalid YAML: {e}",
                    cfg.domain_denylist_path
                ))
            })?;
        }
        // The reputation feed is parsed leniently (no schema), so the only
        // failure mode worth a hard startup error is "configured but
        // unreadable" — surface that here rather than as a silent empty set.
        read_optional_file(&cfg.reputation_source_path)?;
        Ok(())
    }

    /// Build the live guard. Total by design: an individually-bad pattern is
    /// logged and skipped, a missing file is an empty deny-list with a warn.
    /// In practice [`Self::validate`] ran first, so these paths are unreached.
    pub fn from_config(cfg: &AbuseConfig) -> Self {
        Self {
            state: ArcSwap::from_pointee(Self::build_state(cfg)),
        }
    }

    /// Re-read the URL patterns, deny-list file, and reputation feed and
    /// swap them in atomically. The candidate config is validated first, so
    /// a malformed edit (bad regex / unparseable YAML / unreadable feed) is
    /// rejected here and the running rules stay intact — a reload can only
    /// ever replace the live set with a fully-good one, never degrade it.
    /// Triggered by SIGHUP from `main` when `abuse.hot_reload_enabled`.
    pub fn reload(&self, cfg: &AbuseConfig) -> Result<()> {
        Self::validate(cfg)?;
        self.state.store(Arc::new(Self::build_state(cfg)));
        Ok(())
    }

    fn build_state(cfg: &AbuseConfig) -> AbuseState {
        let valid: Vec<&String> = cfg
            .url_patterns_denied
            .iter()
            .filter(|p| match regex::Regex::new(p) {
                Ok(_) => true,
                Err(e) => {
                    tracing::error!(pattern = %p, error = %e, "skipping invalid abuse URL pattern");
                    false
                }
            })
            .collect();
        // `valid` are already known-good, so the case-insensitive set build
        // cannot fail; fall back to an empty set if it somehow does.
        let url_patterns = RegexSet::new(valid.iter().map(|p| format!("(?i){p}")))
            .unwrap_or_else(|_| RegexSet::empty());

        let domain_denylist = match read_optional_file(&cfg.domain_denylist_path) {
            Ok(Some(raw)) => match serde_norway::from_str::<DenylistFile>(&raw) {
                Ok(f) => f
                    .domains
                    .into_iter()
                    .map(|e| e.domain.trim().trim_end_matches('.').to_lowercase())
                    .filter(|d| !d.is_empty())
                    .collect(),
                Err(e) => {
                    tracing::error!(error = %e, "abuse deny-list YAML parse failed; empty deny-list");
                    HashSet::new()
                }
            },
            Ok(None) => {
                tracing::warn!(
                    path = %cfg.domain_denylist_path,
                    "abuse deny-list file absent; domain deny-list empty"
                );
                HashSet::new()
            }
            Err(e) => {
                tracing::error!(error = %e, "abuse deny-list read failed; empty deny-list");
                HashSet::new()
            }
        };

        let reputation = match read_optional_file(&cfg.reputation_source_path) {
            Ok(Some(raw)) => parse_reputation(&raw),
            Ok(None) => {
                if !cfg.reputation_source_path.trim().is_empty() {
                    tracing::warn!(
                        path = %cfg.reputation_source_path,
                        "abuse reputation feed absent; reputation check off"
                    );
                }
                HashSet::new()
            }
            Err(e) => {
                tracing::error!(error = %e, "abuse reputation feed read failed; reputation check off");
                HashSet::new()
            }
        };

        AbuseState {
            url_patterns,
            domain_denylist,
            reputation,
        }
    }

    /// First abuse rule the check trips, if any. URL patterns apply to the
    /// full HTTP URL; the domain deny-list and the reputation feed (both
    /// with parent-domain matching) apply to every check kind's host. One
    /// `ArcSwap` snapshot per call, so a concurrent reload is seen as
    /// fully the old or fully the new set, never a half-updated mix.
    pub fn inspect(&self, check: &CheckSpec) -> Option<AbuseHit> {
        let s = self.state.load();
        // Only the two URL-bearing kinds can trip a full-URL pattern, and it
        // outranks the host rules. Exhaustive on purpose: a new URL-bearing
        // kind must not slip past pattern matching by defaulting to None.
        let url = match check {
            CheckSpec::Http(h) => Some(h.url.as_str()),
            CheckSpec::Flow(f) => Some(f.start_url.as_str()),
            CheckSpec::Tcp(_)
            | CheckSpec::Ping(_)
            | CheckSpec::Heartbeat(_)
            | CheckSpec::TlsCert(_)
            | CheckSpec::DomainExpiry(_)
            | CheckSpec::Dns(_) => None,
        };
        if let Some(u) = url
            && let Some(i) = s.url_patterns.matches(u).iter().next()
        {
            return Some(AbuseHit {
                kind: AbuseKind::UrlPattern,
                detail: format!("pattern #{i}"),
            });
        }
        // An IPv6 literal survives as-is; that's fine — IP literals are never
        // host-set entries (the SSRF guard governs them), so the
        // domain/reputation lookups simply no-op. Heartbeat has no host and
        // drops out here.
        let host = check.primary_host()?;
        s.domain_hit(host).or_else(|| s.reputation_hit(host))
    }

    /// Same rules applied to an arbitrary outbound delivery URL (e.g. a
    /// notification webhook): URL patterns against the full URL, then the
    /// domain deny-list / reputation feed against its host. An unparseable
    /// URL is no hit — transport validation rejects it separately.
    pub fn inspect_url(&self, url: &str) -> Option<AbuseHit> {
        let s = self.state.load();
        if let Some(i) = s.url_patterns.matches(url).iter().next() {
            return Some(AbuseHit {
                kind: AbuseKind::UrlPattern,
                detail: format!("pattern #{i}"),
            });
        }
        let parsed = url::Url::parse(url).ok()?;
        let host = parsed.host_str()?;
        s.domain_hit(host).or_else(|| s.reputation_hit(host))
    }
}

impl AbuseState {
    fn domain_hit(&self, host: &str) -> Option<AbuseHit> {
        Self::match_in(&self.domain_denylist, host, AbuseKind::Domain)
    }

    fn reputation_hit(&self, host: &str) -> Option<AbuseHit> {
        Self::match_in(&self.reputation, host, AbuseKind::Reputation)
    }

    /// `host` or any of its parent domains present in `set` → a hit of
    /// `kind`. Empty set short-circuits so an unconfigured feed costs
    /// nothing on the check path.
    fn match_in(set: &HashSet<String>, host: &str, kind: AbuseKind) -> Option<AbuseHit> {
        if set.is_empty() {
            return None;
        }
        domain_and_parents(host)
            .into_iter()
            .find_map(|d| set.contains(&d).then_some(AbuseHit { kind, detail: d }))
    }
}

/// `host` plus every parent domain down to (and including) the registrable
/// two-label tail — never a bare TLD. `a.b.example.com` →
/// `[a.b.example.com, b.example.com, example.com]`.
pub(crate) fn domain_and_parents(host: &str) -> Vec<String> {
    let host = host.trim().trim_end_matches('.').to_lowercase();
    if host.is_empty() {
        return Vec::new();
    }
    let labels: Vec<&str> = host.split('.').collect();
    (0..labels.len().saturating_sub(1))
        .map(|i| labels[i..].join("."))
        .collect()
}

/// Role/infrastructure mailboxes that loop mail or never consent to receive it.
pub(crate) const RESERVED_EMAIL_LOCALPARTS: &[&str] = &[
    "postmaster",
    "mailer-daemon",
    "abuse",
    "bounce",
    "bounces",
    "no-reply",
    "noreply",
];

/// The platform's own domains (mail `From` domain + app host); a destination
/// here could loop mail through us or impersonate the platform.
pub(crate) fn operator_domains(from_address: &str, public_base_url: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some((_, domain)) = from_address.rsplit_once('@') {
        out.push(domain.to_string());
    }
    if let Some(host) = url::Url::parse(public_base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
    {
        out.push(host);
    }
    out
}

/// Walks the destination's parent chain so a trailing FQDN dot or a `-suffix`
/// lookalike can't slip an operator domain past a naive compare.
pub(crate) fn blocked_email_destination(to: &str, operator_domains: &[String]) -> Option<String> {
    let (local, domain) = to.split_once('@')?;
    let local = local.to_ascii_lowercase();
    // Compare the base mailbox: most MTAs route `postmaster+x` to `postmaster`.
    let base_local = local.split('+').next().unwrap_or(&local);
    if RESERVED_EMAIL_LOCALPARTS.contains(&base_local) {
        return Some(format!(
            "'{base_local}@' is a reserved role address and can't be used"
        ));
    }
    let ops: Vec<String> = operator_domains
        .iter()
        .map(|d| d.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|d| !d.is_empty())
        .collect();
    let parents = domain_and_parents(domain);
    if parents.iter().any(|p| ops.iter().any(|op| op == p)) {
        let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
        return Some(format!(
            "'{domain}' is one of our own domains and can't be used"
        ));
    }
    None
}

/// Reads an optional abuse input file (deny-list or reputation feed).
/// `Ok(None)` = path empty or file absent (not an error — a deployment may
/// legitimately omit it); `Err` = present but unreadable.
fn read_optional_file(path: &str) -> Result<Option<String>> {
    if path.trim().is_empty() {
        return Ok(None);
    }
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AppError::Other(anyhow::anyhow!(
            "abuse: cannot read {path}: {e}"
        ))),
    }
}

/// Parse a hosts-format reputation feed into a host set. Tolerant by
/// design: `#` comments and blanks are skipped, a `0.0.0.0`/`127.0.0.1`
/// sink prefix is stripped, a bare `domain` per line is accepted, and
/// non-domain junk (`localhost`, malformed) is dropped. A malformed feed
/// yields a smaller set, never an error — reputation must never be the
/// reason a deployment fails to boot.
fn parse_reputation(raw: &str) -> HashSet<String> {
    raw.lines()
        .filter_map(|line| {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                return None;
            }
            let mut tok = line.split_whitespace();
            let first = tok.next()?;
            // hosts format prefixes the domain with a sink IP
            // (`0.0.0.0 d`, `127.0.0.1 d`, or any other blocker IP a feed
            // chooses); a feed may also list a bare domain per line. Strip
            // a leading IP of either family rather than allow-list known
            // sinks, so a non-StevenBlack feed isn't silently mis-parsed.
            let domain = if first.parse::<std::net::IpAddr>().is_ok() {
                tok.next()?
            } else {
                first
            };
            let d = domain.trim().trim_end_matches('.').to_lowercase();
            // A registrable host has a dot; this also drops `localhost`
            // and any IP-only line without a separate guard.
            d.contains('.').then_some(d)
        })
        .collect()
}

#[cfg(test)]
impl AbuseHit {
    fn into_app_error_code(self) -> &'static str {
        match self.into_app_error() {
            AppError::BadRequest { code, .. } => code,
            _ => "UNEXPECTED",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard(patterns: &[&str], domains: &[&str]) -> AbuseGuard {
        AbuseGuard {
            state: ArcSwap::from_pointee(AbuseState {
                url_patterns: RegexSet::new(patterns.iter().map(|p| format!("(?i){p}"))).unwrap(),
                domain_denylist: domains.iter().map(|d| d.to_string()).collect(),
                reputation: HashSet::new(),
            }),
        }
    }

    fn rep_guard(reputation: &[&str]) -> AbuseGuard {
        AbuseGuard {
            state: ArcSwap::from_pointee(AbuseState {
                url_patterns: RegexSet::empty(),
                domain_denylist: HashSet::new(),
                reputation: reputation.iter().map(|d| d.to_string()).collect(),
            }),
        }
    }

    fn http(url: &str) -> CheckSpec {
        use crate::domain::{ExpectedStatus, HttpCheck, HttpMethod};
        CheckSpec::Http(HttpCheck {
            url: url.parse().unwrap(),
            method: HttpMethod::Get,
            timeout: std::time::Duration::from_secs(5),
            follow_redirects: false,
            max_redirects: 0,
            expected_status: ExpectedStatus::Exact(200),
            expected_body_contains: None,
            headers: Default::default(),
            body: None,
            verify_tls: true,
            basic_auth: None,
            bearer_token: None,
        })
    }

    #[test]
    fn url_pattern_blocks_git_dir() {
        let g = guard(&[r"/\.git(/|$)"], &[]);
        let hit = g.inspect(&http("https://example.com/.git/config")).unwrap();
        assert_eq!(hit.kind, AbuseKind::UrlPattern);
        assert_eq!(hit.into_app_error_code(), codes::ABUSE_BLOCKED);
    }

    #[test]
    fn url_pattern_is_case_insensitive_and_unanchored() {
        let g = guard(&[r"/wp-admin"], &[]);
        assert!(g.inspect(&http("https://x.com/path/WP-Admin/")).is_some());
    }

    #[test]
    fn clean_url_passes() {
        let g = guard(&[r"/\.git(/|$)"], &["status.betterstack.com"]);
        assert!(g.inspect(&http("https://example.com/healthz")).is_none());
    }

    #[test]
    fn domain_denylist_matches_exact_and_subdomain() {
        let g = guard(&[], &["status.betterstack.com"]);
        let hit = g.inspect(&http("https://status.betterstack.com/")).unwrap();
        assert_eq!(hit.kind, AbuseKind::Domain);
        // Parent-domain match: a sub-subdomain of a denied domain is denied.
        assert!(
            g.inspect(&http("https://eu.status.betterstack.com/"))
                .is_some()
        );
        // A sibling that merely shares the registrable tail is NOT denied
        // (only `status.betterstack.com` and below are listed).
        assert!(g.inspect(&http("https://betterstack.com/")).is_none());
    }

    #[test]
    fn domain_and_parents_stops_at_two_labels() {
        assert_eq!(
            domain_and_parents("a.b.example.com"),
            vec!["a.b.example.com", "b.example.com", "example.com"]
        );
        assert_eq!(domain_and_parents("example.com"), vec!["example.com"]);
        assert!(domain_and_parents("localhost").is_empty());
    }

    #[test]
    fn empty_denylist_never_matches_domain() {
        let g = guard(&[], &[]);
        assert!(
            g.inspect(&http("https://status.betterstack.com/"))
                .is_none()
        );
    }

    #[test]
    fn reload_swaps_in_the_new_denylist_atomically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("denylist.yaml");
        std::fs::write(&path, "domains:\n  - { domain: \"old.example.com\" }\n").unwrap();
        let cfg = AbuseConfig {
            url_patterns_denied: vec![],
            domain_denylist_path: path.to_string_lossy().into_owned(),
            reputation_source_path: String::new(),
            hot_reload_enabled: true,
        };
        let g = AbuseGuard::from_config(&cfg);
        assert!(g.inspect(&http("https://old.example.com/")).is_some());
        assert!(g.inspect(&http("https://new.example.com/")).is_none());

        // An operator edits the file, then SIGHUPs the process.
        std::fs::write(&path, "domains:\n  - { domain: \"new.example.com\" }\n").unwrap();
        g.reload(&cfg).expect("valid reload");

        // The swap is total: the new rule is live, the dropped one is gone.
        assert!(g.inspect(&http("https://new.example.com/")).is_some());
        assert!(g.inspect(&http("https://old.example.com/")).is_none());
        // `dir` drops here, removing the file even on an earlier panic.
    }

    #[test]
    fn reload_rejects_a_bad_edit_and_keeps_the_running_rules() {
        let g = guard(&[r"/\.git(/|$)"], &["blocked.example.com"]);
        assert!(g.inspect(&http("https://blocked.example.com/")).is_some());

        // A malformed regex must not take the live rules down with it.
        let bad = AbuseConfig {
            url_patterns_denied: vec!["(unclosed".to_string()],
            domain_denylist_path: String::new(),
            reputation_source_path: String::new(),
            hot_reload_enabled: true,
        };
        assert!(g.reload(&bad).is_err(), "a bad config must be rejected");
        assert!(
            g.inspect(&http("https://blocked.example.com/")).is_some(),
            "the previous rules must stay intact after a rejected reload"
        );
    }

    #[test]
    fn reputation_blocks_listed_host_and_its_subdomains() {
        let g = rep_guard(&["ads.tracker.example"]);
        let hit = g.inspect(&http("https://ads.tracker.example/x")).unwrap();
        assert_eq!(hit.kind, AbuseKind::Reputation);
        // Generic 400, distinct from the static deny-list's DOMAIN_DENYLISTED.
        assert_eq!(hit.into_app_error_code(), codes::ABUSE_BLOCKED);
        // Parent match: a subdomain of a listed host is blocked too.
        assert!(
            g.inspect(&http("https://eu.ads.tracker.example/"))
                .is_some()
        );
        // A host that merely shares the registrable tail is not listed.
        assert!(g.inspect(&http("https://tracker.example/")).is_none());
        // Reputation also covers non-HTTP check kinds via the host.
        use crate::domain::TcpCheck;
        let tcp = CheckSpec::Tcp(TcpCheck {
            host: "ads.tracker.example".into(),
            port: 443,
            timeout: std::time::Duration::from_secs(5),
        });
        assert!(g.inspect(&tcp).is_some());
    }

    #[test]
    fn parse_reputation_reads_hosts_format_leniently() {
        let raw = "\
# a comment\n\
\n\
0.0.0.0 ad.doubleclick.net\n\
127.0.0.1 tracker.example.com   # trailing comment\n\
10.0.0.1 vendorsink.example\n\
2606:4700:4700::1111 v6sink.example\n\
bare-domain.example\n\
0.0.0.0 localhost\n\
0.0.0.0\n\
not_a_domain\n";
        let set = parse_reputation(raw);
        assert!(set.contains("ad.doubleclick.net"));
        assert!(set.contains("tracker.example.com"));
        // Any sink IP, not just the StevenBlack 0.0.0.0/127.0.0.1.
        assert!(set.contains("vendorsink.example"));
        assert!(set.contains("v6sink.example"));
        assert!(set.contains("bare-domain.example"));
        // `localhost`, the bare sink line, and dot-less junk are dropped.
        assert!(!set.contains("localhost"));
        assert!(!set.contains("not_a_domain"));
        assert_eq!(set.len(), 5);
    }

    #[test]
    fn reload_refreshes_the_reputation_feed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("reputation.hosts");
        std::fs::write(&path, "0.0.0.0 old.bad.example\n").unwrap();
        let cfg = AbuseConfig {
            url_patterns_denied: vec![],
            domain_denylist_path: String::new(),
            reputation_source_path: path.to_string_lossy().into_owned(),
            hot_reload_enabled: true,
        };
        let g = AbuseGuard::from_config(&cfg);
        assert!(g.inspect(&http("https://old.bad.example/")).is_some());

        std::fs::write(&path, "0.0.0.0 new.bad.example\n").unwrap();
        g.reload(&cfg).expect("valid reload");
        assert!(g.inspect(&http("https://new.bad.example/")).is_some());
        assert!(g.inspect(&http("https://old.bad.example/")).is_none());
    }

    fn ops() -> Vec<String> {
        vec!["uptimepage.dev".to_string()]
    }

    #[test]
    fn blocks_role_mailboxes_and_operator_domains() {
        let op = ops();
        assert!(blocked_email_destination("postmaster@example.com", &op).is_some());
        assert!(blocked_email_destination("bounces@customer.io", &op).is_some());
        assert!(blocked_email_destination("alerts@uptimepage.dev", &op).is_some());
        assert!(blocked_email_destination("ops@app.uptimepage.dev", &op).is_some());
        // Trailing FQDN dot does not bypass the domain match.
        assert!(blocked_email_destination("alerts@uptimepage.dev.", &op).is_some());
        // Plus-tagging does not smuggle a role mailbox past the check.
        assert!(blocked_email_destination("postmaster+x@example.com", &op).is_some());
    }

    #[test]
    fn allows_ordinary_destinations() {
        let op = ops();
        assert!(blocked_email_destination("oncall@customer.com", &op).is_none());
        assert!(blocked_email_destination("alerts@customer.io", &op).is_none());
        // Lookalike suffix without a label boundary is not our domain.
        assert!(blocked_email_destination("ops@notuptimepage.dev", &op).is_none());
    }

    #[test]
    fn blocks_apex_even_when_mail_is_sent_from_a_subdomain() {
        let op = vec![
            "mail.uptimepage.dev".to_string(),
            "uptimepage.dev".to_string(),
        ];
        assert!(blocked_email_destination("security@uptimepage.dev", &op).is_some());
        assert!(blocked_email_destination("x@mail.uptimepage.dev", &op).is_some());
    }

    #[test]
    fn empty_operator_list_still_blocks_role_addresses() {
        let op: Vec<String> = vec![];
        assert!(blocked_email_destination("postmaster@uptimepage.dev", &op).is_some());
        assert!(blocked_email_destination("alerts@uptimepage.dev", &op).is_none());
    }

    #[test]
    fn operator_domains_from_config() {
        let d = operator_domains("alerts@uptimepage.dev", "https://app.uptimepage.dev/");
        assert!(d.contains(&"uptimepage.dev".to_string()));
        assert!(d.contains(&"app.uptimepage.dev".to_string()));
    }
}
