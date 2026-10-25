//! Who may open a private status page, and the record of who did.
//!
//! Visitors here are not app users: a viewer is an address that proved itself
//! on one page's host, and nothing about it reaches the app's own sign-in.

use chrono::{DateTime, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{UserId, email_norm, hostname};

/// Plan-independent: the list exists to name people, not to be metered.
pub const MAX_ACCESS_RULES: usize = 500;

/// Bounds what the hash is run over; below it the owner types what they like.
pub const MAX_PAGE_PASSWORD_CHARS: usize = 128;

/// The periods the editor offers. The column CHECKs hold the same lists.
pub const RECHECK_DAY_CHOICES: &[i32] = &[30, 90, 365];
pub const SESSION_DAY_CHOICES: &[i32] = &[7, 30, 90];

macro_rules! db_enum {
    ($(#[$meta:meta])* $name:ident, $check:literal { $($variant:ident => $db:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            #[doc = concat!("Declaration order; the enum-drift test pins this against `", $check, "`.")]
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn as_db_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $db),+
                }
            }

            pub fn from_db_str(s: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|v| v.as_db_str() == s)
            }
        }
    };
}

db_enum!(
    /// An exact address lets in one person; a domain lets in every address at it.
    AccessRuleKind, "status_page_access_rules_kind_check" {
        Address => "address",
        Domain => "domain",
    }
);

db_enum!(
    /// How a session was opened. A password session names no viewer.
    PageSessionMethod, "status_page_sessions_method_check" {
        Email => "email",
        Password => "password",
    }
);

db_enum!(
    AccessEventKind, "status_page_access_events_kind_check" {
        CodeSent => "code_sent",
        CodeFailed => "code_failed",
        SignedIn => "signed_in",
        SignedOut => "signed_out",
        PasskeyAdded => "passkey_added",
        PasskeyRemoved => "passkey_removed",
        Blocked => "blocked",
        Unblocked => "unblocked",
        PasswordFailed => "password_failed",
    }
);

db_enum!(
    AccessEventMethod, "status_page_access_events_method_check" {
        Email => "email",
        Passkey => "passkey",
        Password => "password",
    }
);

/// One entry on a page's access list, already normalised: lower case, no
/// leading `@`, an internationalised domain in its ASCII form.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AccessRule {
    pub kind: AccessRuleKind,
    /// `anna@dealer.example` or `dealer.example`.
    pub value: String,
}

impl AccessRule {
    /// `email` must be normalised the same way the rule was.
    pub fn admits(&self, email: &str) -> bool {
        match self.kind {
            AccessRuleKind::Address => self.value == email,
            AccessRuleKind::Domain => email
                .rsplit_once('@')
                .is_some_and(|(_, domain)| domain == self.value),
        }
    }
}

/// The one spelling a visitor's address and a list entry are compared in:
/// lower case, the domain in its ASCII form. `None` when it is not an address.
pub fn normalize_email(raw: &str) -> Option<String> {
    let email = email_norm::normalize(raw)?;
    let (local, domain) = email.split_once('@')?;
    // `hostname::normalize` would drop a port; an address has none.
    if domain.contains(['@', ':']) {
        return None;
    }
    let domain = hostname::normalize(domain)?;
    Some(format!("{}@{domain}", local.to_lowercase()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RuleError {
    #[error("enter a domain such as example.com or an address such as name@example.com")]
    Malformed,
}

impl AccessRule {
    /// What the owner typed: `dealer.example`, `@dealer.example` or `anna@dealer.example`.
    pub fn parse(raw: &str) -> Result<Self, RuleError> {
        let raw = raw.trim();
        let raw = raw.strip_prefix('@').unwrap_or(raw);
        if raw.contains('@') {
            let value = normalize_email(raw).ok_or(RuleError::Malformed)?;
            return Ok(Self {
                kind: AccessRuleKind::Address,
                value,
            });
        }
        // `hostname::normalize` drops a port, which a domain entry never has.
        if raw.contains(':') {
            return Err(RuleError::Malformed);
        }
        let value = hostname::normalize(raw).ok_or(RuleError::Malformed)?;
        Ok(Self {
            kind: AccessRuleKind::Domain,
            value: value.into_owned(),
        })
    }

    /// A rule as stored or sent, brought to the spelling [`Self::parse`]
    /// produces, so its kind and value cannot disagree.
    pub fn normalized(&self) -> Result<Self, RuleError> {
        let parsed = Self::parse(&self.value)?;
        if parsed.kind == self.kind {
            Ok(parsed)
        } else {
            Err(RuleError::Malformed)
        }
    }
}

/// The owner's settings for one page. `protected` is what makes it private;
/// the rest is kept while it is public so a round trip loses nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageAccessSettings {
    pub protected: bool,
    pub email_enabled: bool,
    pub passkeys_enabled: bool,
    /// Days a passkey viewer may go without proving their address again.
    /// `None` never asks.
    pub recheck_days: Option<i32>,
    /// For email and password sign-ins alike. `None` keeps a session until
    /// sign-out, or until the way in it came through closes.
    pub session_days: Option<i32>,
    /// Off keeps the stored password, so turning it on again brings it back.
    pub password_enabled: bool,
}

impl Default for PageAccessSettings {
    fn default() -> Self {
        Self {
            protected: false,
            email_enabled: true,
            passkeys_enabled: true,
            recheck_days: None,
            session_days: None,
            password_enabled: false,
        }
    }
}

/// What the gate reads for one page. Never the password or its hash.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageAccess {
    pub settings: PageAccessSettings,
    pub rules: Vec<AccessRule>,
    /// When the stored password was set; `None` while none ever was. A
    /// subscription made with a password carries the same instant.
    pub password_set_at: Option<DateTime<Utc>>,
}

impl PageAccess {
    pub fn is_private(&self) -> bool {
        self.settings.protected
    }

    /// `email` must be normalised the same way the rules were.
    pub fn admits(&self, email: &str) -> bool {
        self.settings.email_enabled && self.rules.iter().any(|r| r.admits(email))
    }

    pub fn has_password(&self) -> bool {
        self.password_set_at.is_some()
    }

    /// The subscription was made with the password stored now.
    pub fn made_with_password(&self, via_password_at: Option<DateTime<Utc>>) -> bool {
        via_password_at.is_some() && via_password_at == self.password_set_at
    }

    /// A subscription stays while a way in still covers it: the email list
    /// its address is on, or the password it was made with, still on.
    pub fn keeps_subscription(
        &self,
        email: Option<&str>,
        via_password_at: Option<DateTime<Utc>>,
    ) -> bool {
        email.is_some_and(|e| self.admits(e))
            || (self.settings.password_enabled && self.made_with_password(via_password_at))
    }
}

/// Who may open a page, as the API, MCP and Terraform send and read it.
/// Replaces the stored settings and list whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AccessSpec {
    /// Private when true: only people a way in lets through can open the
    /// page. False keeps the list and the password for the next time it is
    /// made private.
    pub protected: bool,
    /// Keep a sign-in, by email or password, for this many days: 7, 30 or
    /// 90. `null` keeps it until sign-out, or until the way in it came
    /// through closes.
    #[serde(default)]
    pub session_days: Option<i32>,
    #[serde(default)]
    pub email: EmailAccessSpec,
    #[serde(default)]
    pub password: PasswordAccessSpec,
    /// Who can get an email code, up to 500 entries.
    #[serde(default)]
    pub rules: Vec<AccessRule>,
}

/// The emailed-code way in, and the passkeys that shortcut it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EmailAccessSpec {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Viewers may save a passkey after an email sign-in and use it next time.
    #[serde(default = "yes")]
    pub passkeys_enabled: bool,
    /// Ask passkey viewers for an email code again after this many days: 30,
    /// 90 or 365. `null` never asks.
    #[serde(default)]
    pub recheck_days: Option<i32>,
}

fn yes() -> bool {
    true
}

impl Default for EmailAccessSpec {
    fn default() -> Self {
        let s = PageAccessSettings::default();
        Self {
            enabled: s.email_enabled,
            passkeys_enabled: s.passkeys_enabled,
            recheck_days: s.recheck_days,
        }
    }
}

/// One password shared with everyone who should read the page. It is stored
/// as a hash, so it is never read back, only replaced.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PasswordAccessSpec {
    /// Anyone with the password can open the page. Off keeps the stored
    /// password, so turning it on again brings the same one back.
    #[serde(default)]
    pub enabled: bool,
    /// Sets or replaces the password, kept exactly as sent: 1 to 128
    /// characters. Write only, never returned; omit it to keep the stored one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, write_only, min_length = 1, max_length = 128)]
    pub value: Option<PagePassword>,
}

/// The page password as the owner sees it. Never the password itself, which
/// is stored as a hash and can only be replaced.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct PagePasswordView {
    pub enabled: bool,
    /// When the stored password was set, or `null` while there is none.
    #[schema(nullable = true)]
    pub set_at: Option<DateTime<Utc>>,
    /// Who set it, while they have an account.
    #[schema(nullable = true)]
    pub set_by: Option<String>,
    /// Devices signed in with it now.
    pub sessions: i64,
    #[schema(nullable = true)]
    pub last_sign_in_at: Option<DateTime<Utc>>,
    /// Wrong tries in the last 24 hours.
    pub failed_last_24h: i64,
    /// Subscriptions made with it that the email list does not cover: what a
    /// new password or turning it off ends.
    pub subscriptions: i64,
}

/// A page password as the owner typed it. Prints as redacted, so no `Debug`
/// or log line carries it.
#[derive(Clone, Deserialize)]
#[serde(transparent)]
pub struct PagePassword(SecretString);

impl PagePassword {
    pub fn expose(&self) -> &str {
        self.0.expose_secret()
    }

    pub fn into_secret(self) -> SecretString {
        self.0
    }
}

impl From<&str> for PagePassword {
    fn from(s: &str) -> Self {
        Self(SecretString::from(s))
    }
}

impl std::fmt::Debug for PagePassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PagePassword([REDACTED])")
    }
}

/// Reads never carry one, so this only stops a stray serialisation from
/// writing it out.
impl Serialize for PagePassword {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("[REDACTED]")
    }
}

impl PartialEq for PagePassword {
    fn eq(&self, other: &Self) -> bool {
        self.expose() == other.expose()
    }
}

impl Eq for PagePassword {}

impl From<&PageAccess> for AccessSpec {
    fn from(a: &PageAccess) -> Self {
        let s = a.settings;
        Self {
            protected: s.protected,
            session_days: s.session_days,
            email: EmailAccessSpec {
                enabled: s.email_enabled,
                passkeys_enabled: s.passkeys_enabled,
                recheck_days: s.recheck_days,
            },
            password: PasswordAccessSpec {
                enabled: s.password_enabled,
                value: None,
            },
            rules: a.rules.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccessError {
    #[error("access.rules[{index}]: {source}")]
    Rule { index: usize, source: RuleError },
    #[error("a page lists at most {MAX_ACCESS_RULES} addresses and domains")]
    TooManyRules,
    #[error("{field} must be one of {choices:?} or null")]
    Period {
        field: &'static str,
        choices: &'static [i32],
    },
    #[error("a private page needs a way in: turn on the email code or set a password")]
    NoWayIn,
    #[error(
        "add who can get a code: with the email code on, the list needs at least one address or domain"
    )]
    EmptyList,
    #[error("set a password before turning the password way in on")]
    PasswordRequired,
    #[error("a page password is 1 to {MAX_PAGE_PASSWORD_CHARS} characters")]
    InvalidPassword,
    #[error("private status pages are part of the Team plan")]
    NotOnPlan,
}

impl AccessError {
    pub fn field(&self) -> String {
        match self {
            Self::Rule { index, .. } => format!("access.rules[{index}]"),
            Self::TooManyRules | Self::EmptyList => "access.rules".into(),
            Self::Period { field, .. } => (*field).into(),
            Self::NoWayIn => "access.email.enabled".into(),
            Self::PasswordRequired | Self::InvalidPassword => "access.password.value".into(),
            Self::NotOnPlan => "access.protected".into(),
        }
    }
}

/// A checked access write, and the new password it carries, still in the
/// clear: the caller hashes it off the async runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidAccess {
    pub access: PageAccess,
    pub password: Option<PagePassword>,
}

impl AccessSpec {
    /// The write this spec asks for, normalised, or why it cannot be made.
    /// `plan_allows` is whether the plan includes private pages; a page that
    /// already is one stays editable while held for a plan that lost them.
    /// `stored` is the page's access now, whose password a spec without a
    /// value keeps.
    pub fn validate(
        self,
        plan_allows: bool,
        stored: Option<&PageAccess>,
    ) -> Result<ValidAccess, AccessError> {
        // Before any parsing, so an oversized list costs nothing to refuse.
        if self.rules.len() > MAX_ACCESS_RULES {
            return Err(AccessError::TooManyRules);
        }
        let mut seen = std::collections::HashSet::with_capacity(self.rules.len());
        let mut rules: Vec<AccessRule> = Vec::with_capacity(self.rules.len());
        for (index, rule) in self.rules.iter().enumerate() {
            let rule = rule
                .normalized()
                .map_err(|source| AccessError::Rule { index, source })?;
            if seen.insert(rule.clone()) {
                rules.push(rule);
            }
        }
        for (value, field, choices) in [
            (
                self.email.recheck_days,
                "access.email.recheck_days",
                RECHECK_DAY_CHOICES,
            ),
            (
                self.session_days,
                "access.session_days",
                SESSION_DAY_CHOICES,
            ),
        ] {
            if value.is_some_and(|v| !choices.contains(&v)) {
                return Err(AccessError::Period { field, choices });
            }
        }
        let password = self.password.value;
        if password.as_ref().is_some_and(|p| {
            p.expose().is_empty() || p.expose().chars().count() > MAX_PAGE_PASSWORD_CHARS
        }) {
            return Err(AccessError::InvalidPassword);
        }
        if self.password.enabled
            && password.is_none()
            && !stored.is_some_and(PageAccess::has_password)
        {
            return Err(AccessError::PasswordRequired);
        }
        if self.protected {
            if !plan_allows && !stored.is_some_and(PageAccess::is_private) {
                return Err(AccessError::NotOnPlan);
            }
            if !self.email.enabled && !self.password.enabled {
                return Err(AccessError::NoWayIn);
            }
            if self.email.enabled && rules.is_empty() {
                return Err(AccessError::EmptyList);
            }
        }
        Ok(ValidAccess {
            access: PageAccess {
                settings: PageAccessSettings {
                    protected: self.protected,
                    email_enabled: self.email.enabled,
                    passkeys_enabled: self.email.passkeys_enabled,
                    recheck_days: self.email.recheck_days,
                    session_days: self.session_days,
                    password_enabled: self.password.enabled,
                },
                rules,
                password_set_at: stored.and_then(|s| s.password_set_at),
            },
            password,
        })
    }
}

/// A validated access write, carried with the page edit it arrives in so both
/// commit together.
#[derive(Debug, Clone)]
pub struct AccessWrite {
    pub access: PageAccess,
    /// The new password's argon2id PHC string; `None` keeps the stored one.
    pub password_hash: Option<String>,
    pub actor: Option<UserId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(kind: AccessRuleKind, value: &str) -> AccessRule {
        AccessRule {
            kind,
            value: value.into(),
        }
    }

    #[test]
    fn a_domain_admits_every_address_at_it_and_nothing_below_it() {
        let r = rule(AccessRuleKind::Domain, "dealer.example");
        assert!(r.admits("anna@dealer.example"));
        assert!(!r.admits("anna@mail.dealer.example"));
        assert!(!r.admits("anna@notdealer.example"));
        assert!(!r.admits("dealer.example"));
    }

    #[test]
    fn an_address_admits_only_itself() {
        let r = rule(AccessRuleKind::Address, "anna@dealer.example");
        assert!(r.admits("anna@dealer.example"));
        assert!(!r.admits("bob@dealer.example"));
    }

    #[test]
    fn a_page_with_email_off_admits_nobody_by_email() {
        let mut access = PageAccess {
            settings: PageAccessSettings {
                protected: true,
                ..Default::default()
            },
            rules: vec![rule(AccessRuleKind::Domain, "dealer.example")],
            password_set_at: None,
        };
        assert!(access.admits("anna@dealer.example"));
        access.settings.email_enabled = false;
        assert!(!access.admits("anna@dealer.example"));
    }

    #[test]
    fn an_entry_is_read_as_typed_and_stored_in_one_spelling() {
        assert_eq!(
            AccessRule::parse(" @Dealer.EXAMPLE ").unwrap(),
            rule(AccessRuleKind::Domain, "dealer.example")
        );
        assert_eq!(
            AccessRule::parse("Anna@Bücher.de").unwrap(),
            rule(AccessRuleKind::Address, "anna@xn--bcher-kva.de")
        );
        for bad in [
            "",
            "@",
            "dealer",
            "dealer.example:443",
            "a@b",
            "anna@",
            "two words.de",
            "a@@dealer.example",
            "anna@dealer.example:443",
        ] {
            assert!(AccessRule::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_rule_whose_kind_and_value_disagree_is_refused() {
        assert!(
            rule(AccessRuleKind::Domain, "anna@dealer.example")
                .normalized()
                .is_err()
        );
        assert!(
            rule(AccessRuleKind::Address, "dealer.example")
                .normalized()
                .is_err()
        );
        assert_eq!(
            rule(AccessRuleKind::Domain, "DEALER.example")
                .normalized()
                .unwrap(),
            rule(AccessRuleKind::Domain, "dealer.example")
        );
    }

    #[test]
    fn a_visitor_address_matches_a_rule_typed_in_another_case_or_script() {
        let email = normalize_email("ANNA@bücher.de").unwrap();
        assert!(AccessRule::parse("Bücher.de").unwrap().admits(&email));
        assert!(AccessRule::parse("anna@BÜCHER.de").unwrap().admits(&email));
    }

    fn spec(protected: bool, rules: &[(&str, AccessRuleKind)]) -> AccessSpec {
        AccessSpec {
            protected,
            session_days: None,
            email: EmailAccessSpec::default(),
            password: PasswordAccessSpec::default(),
            rules: rules.iter().map(|(v, k)| rule(*k, v)).collect(),
        }
    }

    fn password_only(value: Option<&str>) -> AccessSpec {
        let mut s = spec(true, &[]);
        s.email.enabled = false;
        s.password = PasswordAccessSpec {
            enabled: true,
            value: value.map(PagePassword::from),
        };
        s
    }

    fn stored_password() -> PageAccess {
        PageAccess {
            password_set_at: Some(Utc::now()),
            ..Default::default()
        }
    }

    #[test]
    fn a_private_page_needs_the_plan_a_way_in_and_someone_on_the_list() {
        let ok = spec(true, &[("dealer.example", AccessRuleKind::Domain)]);
        assert!(ok.clone().validate(true, None).is_ok());
        assert_eq!(
            ok.clone().validate(false, None),
            Err(AccessError::NotOnPlan)
        );
        assert_eq!(
            spec(true, &[]).validate(true, None),
            Err(AccessError::EmptyList)
        );
        let mut off = ok;
        off.email.enabled = false;
        assert_eq!(off.validate(true, None), Err(AccessError::NoWayIn));
        // Public keeps its list, empty or not, whatever the plan.
        assert!(spec(false, &[]).validate(false, None).is_ok());
    }

    #[test]
    fn a_page_held_since_its_plan_lost_private_pages_stays_editable() {
        let ok = spec(true, &[("dealer.example", AccessRuleKind::Domain)]);
        let held = PageAccess {
            settings: PageAccessSettings {
                protected: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(ok.clone().validate(false, Some(&held)).is_ok());
        assert_eq!(
            ok.validate(false, Some(&PageAccess::default())),
            Err(AccessError::NotOnPlan)
        );
    }

    #[test]
    fn a_password_alone_is_a_way_in_and_needs_no_list() {
        let valid = password_only(Some("x")).validate(true, None).unwrap();
        assert!(valid.access.settings.password_enabled);
        assert!(!valid.access.settings.email_enabled);
        assert_eq!(valid.password, Some(PagePassword::from("x")));
        // With email on as well, the list is needed again.
        let mut both = password_only(Some("x"));
        both.email.enabled = true;
        assert_eq!(both.validate(true, None), Err(AccessError::EmptyList));
    }

    #[test]
    fn turning_the_password_on_needs_one_sent_or_stored() {
        assert_eq!(
            password_only(None).validate(true, None),
            Err(AccessError::PasswordRequired)
        );
        let kept = password_only(None)
            .validate(true, Some(&stored_password()))
            .unwrap();
        assert_eq!(kept.password, None);
        assert!(kept.access.has_password());
        // Stored while off is fine without a way in being on yet.
        let mut stored_off = spec(false, &[]);
        stored_off.password.value = Some(PagePassword::from("later"));
        assert!(stored_off.validate(true, None).is_ok());
    }

    #[test]
    fn a_password_is_kept_as_typed_and_bounded_only_by_length() {
        for ok in [
            "1",
            " padded ",
            "ünïcödé",
            &"a".repeat(MAX_PAGE_PASSWORD_CHARS),
        ] {
            let valid = password_only(Some(ok)).validate(true, None).unwrap();
            assert_eq!(valid.password.unwrap().expose(), ok);
        }
        // Characters, not bytes: 128 multi-byte ones still fit.
        assert!(
            password_only(Some(&"ü".repeat(MAX_PAGE_PASSWORD_CHARS)))
                .validate(true, None)
                .is_ok()
        );
        for bad in ["", &"a".repeat(MAX_PAGE_PASSWORD_CHARS + 1)] {
            assert_eq!(
                password_only(Some(bad)).validate(true, None),
                Err(AccessError::InvalidPassword)
            );
        }
    }

    #[test]
    fn the_password_never_prints_and_never_reads_back() {
        let p = PagePassword::from("hunter2-secret");
        assert!(!format!("{p:?}").contains("hunter2"));
        let spec: AccessSpec = serde_json::from_value(serde_json::json!({
            "protected": true,
            "email": { "enabled": false },
            "password": { "enabled": true, "value": "hunter2-secret" },
        }))
        .unwrap();
        assert!(!format!("{spec:?}").contains("hunter2"));
        assert!(!serde_json::to_string(&spec).unwrap().contains("hunter2"));
        let read = AccessSpec::from(&spec.validate(true, None).unwrap().access);
        let out = serde_json::to_value(&read).unwrap();
        assert_eq!(out["password"], serde_json::json!({ "enabled": true }));
    }

    #[test]
    fn a_subscription_stays_while_a_way_in_covers_it() {
        let set_at = Utc::now();
        let mut access = PageAccess {
            settings: PageAccessSettings {
                protected: true,
                password_enabled: true,
                ..Default::default()
            },
            rules: vec![rule(AccessRuleKind::Domain, "dealer.example")],
            password_set_at: Some(set_at),
        };
        assert!(access.keeps_subscription(Some("anna@dealer.example"), None));
        assert!(access.keeps_subscription(Some("j@gmail.example"), Some(set_at)));
        assert!(!access.keeps_subscription(Some("j@gmail.example"), None));
        let older = set_at - chrono::Duration::days(1);
        assert!(!access.keeps_subscription(Some("j@gmail.example"), Some(older)));
        assert!(!access.keeps_subscription(None, None));
        access.settings.password_enabled = false;
        assert!(!access.keeps_subscription(Some("j@gmail.example"), Some(set_at)));
    }

    #[test]
    fn the_list_is_normalised_deduplicated_and_capped() {
        let access = spec(
            true,
            &[
                ("DEALER.example", AccessRuleKind::Domain),
                ("dealer.example", AccessRuleKind::Domain),
            ],
        )
        .validate(true, None)
        .unwrap()
        .access;
        assert_eq!(
            access.rules,
            vec![rule(AccessRuleKind::Domain, "dealer.example")]
        );
        let bad = spec(true, &[("dealer.example", AccessRuleKind::Address)]);
        assert!(matches!(
            bad.validate(true, None),
            Err(AccessError::Rule { index: 0, .. })
        ));
        let many: Vec<String> = (0..=MAX_ACCESS_RULES).map(|i| format!("d{i}.de")).collect();
        let many: Vec<(&str, AccessRuleKind)> = many
            .iter()
            .map(|d| (d.as_str(), AccessRuleKind::Domain))
            .collect();
        assert_eq!(
            spec(true, &many).validate(true, None),
            Err(AccessError::TooManyRules)
        );
    }

    #[test]
    fn only_the_offered_periods_are_accepted() {
        let mut s = spec(true, &[("dealer.example", AccessRuleKind::Domain)]);
        s.email.recheck_days = Some(14);
        assert!(matches!(
            s.clone().validate(true, None),
            Err(AccessError::Period { .. })
        ));
        s.email.recheck_days = Some(90);
        s.session_days = Some(31);
        assert_eq!(
            s.clone().validate(true, None).unwrap_err().field(),
            "access.session_days"
        );
        s.session_days = Some(30);
        assert!(s.validate(true, None).is_ok());
    }

    #[test]
    fn db_strings_round_trip() {
        for k in AccessEventKind::ALL {
            assert_eq!(AccessEventKind::from_db_str(k.as_db_str()), Some(*k));
        }
        assert_eq!(AccessRuleKind::from_db_str("nope"), None);
    }
}
