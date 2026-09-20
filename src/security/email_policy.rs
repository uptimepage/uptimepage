//! Whether an address may open an account or receive mail we send.
//!
//! Two signals: membership of a disposable-domain corpus, and whether the
//! domain publishes a mail exchanger. Neither is authoritative — lists lag on
//! new burners and overreach on relays (today's corpus lists `addy.io`), so
//! [`NEVER_DISPOSABLE`] is the floor no upstream edit can cross.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use arc_swap::ArcSwap;
use hickory_resolver::proto::rr::{RData, RecordType};

use crate::config::{EmailPolicyConfig, SignupPolicy};
use crate::error::AppError;
use crate::error::codes;
use crate::http_client::HickoryDnsResolver;
use crate::metric_names;
use crate::security::abuse::domain_and_parents;

/// Cached by the shared resolver, so this is paid once per domain per TTL.
const MX_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// Domains no upstream list may mark disposable, matched with the same parent
/// walk as the corpus. Relays are the reason this exists: they are permanent
/// aliases, they already appear on stricter upstream variants, and someone
/// signing in with Apple has no other address to offer.
const NEVER_DISPOSABLE: &[&str] = &[
    // Mainstream mail
    "gmail.com",
    "googlemail.com",
    "outlook.com",
    "hotmail.com",
    "live.com",
    "msn.com",
    "yahoo.com",
    "ymail.com",
    "icloud.com",
    "me.com",
    "mac.com",
    "aol.com",
    "proton.me",
    "protonmail.com",
    "pm.me",
    "gmx.com",
    "gmx.de",
    "gmx.net",
    "web.de",
    "mail.com",
    "zoho.com",
    "zohomail.com",
    "fastmail.com",
    "hey.com",
    "tutanota.com",
    "tuta.com",
    "ukr.net",
    "meta.ua",
    "i.ua",
    "qq.com",
    "163.com",
    "126.com",
    "naver.com",
    "seznam.cz",
    "orange.fr",
    "free.fr",
    "libero.it",
    "t-online.de",
    // Privacy relays: permanent aliases, not burners.
    "privaterelay.appleid.com",
    "simplelogin.com",
    "simplelogin.co",
    "simplelogin.fr",
    "slmail.me",
    "aleeas.com",
    "8shield.net",
    "addy.io",
    "anonaddy.com",
    "anonaddy.me",
    "mozmail.com",
    "duck.com",
    "relay.firefox.com",
    // Public suffixes: listing one would blanket every domain beneath it.
    "co.uk",
    "org.uk",
    "me.uk",
    "com.au",
    "net.au",
    "org.au",
    "co.nz",
    "co.za",
    "co.jp",
    "or.jp",
    "co.kr",
    "co.in",
    "com.br",
    "com.mx",
    "com.ar",
    "com.tr",
    "com.ua",
    "com.pl",
    "com.cn",
    "com.sg",
    "com.hk",
    "co.il",
];

/// Consulted once per incoming domain on every refresh, so not a linear scan.
static PINNED: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| NEVER_DISPOSABLE.iter().copied().collect());

/// Persisted verbatim as `users.email_risk`, so the strings are schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmailRisk {
    /// The domain is in the disposable corpus.
    Disposable,
    /// The domain publishes no mail exchanger, or an RFC 7505 null MX.
    NoMx,
}

impl EmailRisk {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Disposable => "disposable",
            Self::NoMx => "no_mx",
        }
    }

    /// Says what to do rather than naming the list: "you're on a blocklist"
    /// invites an argument we cannot settle.
    pub fn message(self) -> &'static str {
        match self {
            Self::Disposable => {
                "That looks like a temporary email address. Use one you'll still \
                 be able to read when we send you an alert."
            }
            Self::NoMx => {
                "That domain doesn't accept email, so we'd have no way to reach \
                 you. Check the spelling."
            }
        }
    }

    pub fn into_app_error(self, field: &str) -> AppError {
        AppError::bad_request_field(codes::EMAIL_DESTINATION_BLOCKED, self.message(), field)
    }
}

/// Carried as a value so each surface can spend it at the point it knows the
/// account is new. Someone whose domain was listed after they joined must keep
/// signing in, so this cannot be enforced at the edge of the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Clear,
    Flag(EmailRisk),
    Refuse(EmailRisk),
}

/// `Clear` records nothing — the signal is the refusal rate, and the
/// denominator is already in the HTTP request metrics.
pub(crate) fn record(surface: &'static str, outcome: &'static str, risk: EmailRisk) {
    metrics::counter!(
        metric_names::EMAIL_ADMISSION,
        "surface" => surface,
        "outcome" => outcome,
        "risk" => risk.as_db_str(),
    )
    .increment(1);
}

impl Admission {
    /// For a caller that opens the account whatever the verdict was. A refusal
    /// still yields its risk: the invite path admits regardless of policy, and
    /// dropping the mark there would hide exactly what a `block` operator asked
    /// to be strict about.
    pub fn record_and_take(self, surface: &'static str) -> Option<EmailRisk> {
        match self {
            Self::Flag(r) | Self::Refuse(r) => {
                record(surface, "flagged", r);
                Some(r)
            }
            Self::Clear => None,
        }
    }

    pub fn allow_new(
        self,
        field: &str,
        surface: &'static str,
    ) -> crate::error::Result<Option<EmailRisk>> {
        match self {
            Self::Refuse(r) => {
                record(surface, "refused", r);
                Err(r.into_app_error(field))
            }
            other => Ok(other.record_and_take(surface)),
        }
    }
}

/// One instance per process, shared through `AppState`.
pub struct EmailPolicy {
    enabled: bool,
    require_mx: bool,
    domains: ArcSwap<HashSet<String>>,
}

impl EmailPolicy {
    pub fn from_config(cfg: &EmailPolicyConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            require_mx: cfg.enabled && cfg.require_mx,
            domains: ArcSwap::from_pointee(HashSet::new()),
        }
    }

    /// Filtered here rather than at the source, so the floor holds however the
    /// set was assembled.
    pub fn install(&self, domains: HashSet<String>) -> usize {
        let kept: HashSet<String> = domains
            .into_iter()
            .filter(|d| !PINNED.contains(d.as_str()))
            .collect();
        let n = kept.len();
        self.domains.store(Arc::new(kept));
        n
    }

    pub fn loaded(&self) -> usize {
        self.domains.load().len()
    }

    pub fn disposable_domain(&self, email: &str) -> Option<String> {
        let domain = self.candidate_domain(email)?;
        self.listed(&domain)
    }

    /// `None` when no verdict is possible: feature off, no domain, or pinned.
    fn candidate_domain(&self, email: &str) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let domain = domain_of(email)?;
        (!PINNED.contains(domain.as_str())).then_some(domain)
    }

    fn listed(&self, domain: &str) -> Option<String> {
        let set = self.domains.load();
        if set.is_empty() {
            return None;
        }
        domain_and_parents(domain)
            .into_iter()
            .find(|d| set.contains(d))
    }

    /// Corpus first (free), then MX. Fails open: only an authoritative "takes
    /// no mail" is a verdict, because a DNS blip must not close signups.
    pub async fn assess(&self, email: &str, resolver: &HickoryDnsResolver) -> Option<EmailRisk> {
        let domain = self.candidate_domain(email)?;
        if self.listed(&domain).is_some() {
            return Some(EmailRisk::Disposable);
        }
        if !self.require_mx {
            return None;
        }
        match accepts_mail(resolver, &domain).await {
            Some(false) => Some(EmailRisk::NoMx),
            Some(true) | None => None,
        }
    }
}

impl EmailPolicy {
    pub async fn admit(
        &self,
        email: &str,
        resolver: &HickoryDnsResolver,
        policy: SignupPolicy,
    ) -> Admission {
        if policy == SignupPolicy::Allow {
            return Admission::Clear;
        }
        match self.assess(email, resolver).await {
            None => Admission::Clear,
            Some(risk) => match policy {
                SignupPolicy::Block => Admission::Refuse(risk),
                // Nowhere to deliver: the account could never receive the
                // alerts it exists to send.
                SignupPolicy::Flag if risk == EmailRisk::NoMx => Admission::Refuse(risk),
                SignupPolicy::Flag | SignupPolicy::Allow => Admission::Flag(risk),
            },
        }
    }
}

/// Trailing FQDN dot stripped so `a@example.com.` cannot walk past a match.
pub fn domain_of(email: &str) -> Option<String> {
    let (_, domain) = email.rsplit_once('@')?;
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    (!domain.is_empty()).then_some(domain)
}

/// `Some(false)` is authoritative "takes no mail"; `None` is "resolver could
/// not say" and must not be read as a refusal.
async fn accepts_mail(resolver: &HickoryDnsResolver, domain: &str) -> Option<bool> {
    let mx = tokio::time::timeout(
        MX_LOOKUP_TIMEOUT,
        resolver.inner().lookup(domain, RecordType::MX),
    )
    .await;
    match mx {
        Ok(Ok(lookup)) => {
            let mut null_mx = false;
            for record in lookup.answers() {
                if let RData::MX(mx) = &record.data {
                    if mx.exchange.is_root() {
                        null_mx = true;
                    } else {
                        return Some(true);
                    }
                }
            }
            // RFC 7505: a lone `0 .` exchange is an explicit refusal to accept
            // mail, and must not fall through to the implicit-A rule below.
            if null_mx {
                return Some(false);
            }
        }
        // No MX is not yet a verdict — RFC 5321 §5.1 makes the address record
        // the implicit exchanger.
        Ok(Err(e)) if e.is_no_records_found() => {}
        Ok(Err(_)) | Err(_) => return None,
    }
    // `lookup_ip`, not the check wrapper: its error kinds separate "no such
    // name" from "the resolver fell over". The wrapper flattens both into
    // `anyhow`, and confusing them here would fail closed.
    match tokio::time::timeout(MX_LOOKUP_TIMEOUT, resolver.inner().lookup_ip(domain)).await {
        Ok(Ok(lookup)) => Some(lookup.iter().next().is_some()),
        Ok(Err(e)) if e.is_no_records_found() => Some(false),
        Ok(Err(_)) | Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(domains: &[&str]) -> EmailPolicy {
        let cfg = EmailPolicyConfig {
            enabled: true,
            require_mx: false,
            ..Default::default()
        };
        let p = EmailPolicy::from_config(&cfg);
        p.install(domains.iter().map(|d| (*d).to_string()).collect());
        p
    }

    #[test]
    fn matches_a_listed_domain_and_its_subdomains() {
        let p = policy(&["mailinator.com"]);
        assert_eq!(
            p.disposable_domain("a@mailinator.com").as_deref(),
            Some("mailinator.com")
        );
        assert_eq!(
            p.disposable_domain("a@team.mailinator.com").as_deref(),
            Some("mailinator.com")
        );
        assert!(p.disposable_domain("a@example.com").is_none());
    }

    #[test]
    fn a_trailing_dot_cannot_walk_past_the_match() {
        let p = policy(&["mailinator.com"]);
        assert!(p.disposable_domain("a@mailinator.com.").is_some());
        assert!(p.disposable_domain("a@MAILINATOR.COM").is_some());
    }

    #[test]
    fn the_pinned_floor_survives_a_poisoned_upstream() {
        // Today's upstream really does list addy.io.
        let p = policy(&["gmail.com", "addy.io", "co.uk", "privaterelay.appleid.com"]);
        assert!(p.disposable_domain("a@gmail.com").is_none());
        assert!(p.disposable_domain("a@addy.io").is_none());
        assert!(p.disposable_domain("a@shop.co.uk").is_none());
        assert!(p.disposable_domain("a@privaterelay.appleid.com").is_none());
        assert_eq!(p.loaded(), 0);
    }

    #[test]
    fn an_empty_corpus_blocks_nothing() {
        let p = policy(&[]);
        assert!(p.disposable_domain("a@mailinator.com").is_none());
    }

    #[test]
    fn the_switch_being_off_blocks_nothing() {
        let cfg = EmailPolicyConfig {
            enabled: false,
            ..Default::default()
        };
        let p = EmailPolicy::from_config(&cfg);
        p.install(std::iter::once("mailinator.com".to_string()).collect());
        assert!(p.disposable_domain("a@mailinator.com").is_none());
    }

    #[test]
    fn a_bare_tld_is_never_reachable_by_the_parent_walk() {
        let p = policy(&["com"]);
        assert!(p.disposable_domain("a@example.com").is_none());
    }

    #[test]
    fn addresses_without_a_domain_are_not_our_problem() {
        assert!(domain_of("no-at-sign").is_none());
        assert!(domain_of("trailing@").is_none());
        assert_eq!(domain_of("a@b@example.com").as_deref(), Some("example.com"));
    }
}
