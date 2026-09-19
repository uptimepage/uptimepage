//! One-shot flash cookie: a short-lived, server-set signal that a banner
//! should render exactly once on the next page load. Unlike a query param it
//! can't be forged by editing the URL and doesn't linger across refreshes or
//! get shared in a copied link.

use tower_cookies::Cookie;
use tower_cookies::Cookies;
use tower_cookies::cookie::SameSite;
use tower_cookies::cookie::time::Duration;

use crate::domain::OauthProvider;

const COOKIE_NAME: &str = "_sm_flash";
// Long enough to survive the post-login redirect chain, short enough that it
// never resurfaces on a later visit if the consuming page is somehow skipped.
const TTL_SECS: i64 = 60;

/// Flags carried by a single flash. Extend as new one-shot banners appear.
#[derive(Debug, Default, Clone)]
pub struct Flash {
    pub restored: bool,
    pub invite_missed: bool,
    /// A provider was just added to the account from its own settings page.
    pub identity_linked: Option<OauthProvider>,
    /// The provider account offered for linking already opens a different
    /// account here.
    pub identity_taken: bool,
    /// The dance came back with that provider already on the account.
    pub identity_already_linked: bool,
    /// The provider could not be reached, so nothing was added.
    pub link_failed: bool,
}

impl Flash {
    fn is_empty(&self) -> bool {
        !self.restored
            && !self.invite_missed
            && !self.identity_taken
            && !self.identity_already_linked
            && !self.link_failed
            && self.identity_linked.is_none()
    }

    fn encode(&self) -> String {
        let mut tags: Vec<String> = Vec::new();
        if self.restored {
            tags.push("restored".into());
        }
        if self.invite_missed {
            tags.push("invite_missed".into());
        }
        if self.identity_taken {
            tags.push("identity_taken".into());
        }
        if self.identity_already_linked {
            tags.push("identity_already_linked".into());
        }
        if self.link_failed {
            tags.push("link_failed".into());
        }
        if let Some(p) = self.identity_linked {
            tags.push(format!("identity_linked:{}", p.as_db_str()));
        }
        tags.join(",")
    }

    fn decode(value: &str) -> Self {
        let mut f = Self::default();
        for tag in value.split(',') {
            let tag = tag.trim();
            match tag {
                "restored" => f.restored = true,
                "invite_missed" => f.invite_missed = true,
                "identity_taken" => f.identity_taken = true,
                "identity_already_linked" => f.identity_already_linked = true,
                "link_failed" => f.link_failed = true,
                // Through the enum, so a hand-written cookie cannot put words
                // on the page.
                _ => {
                    if let Some(slug) = tag.strip_prefix("identity_linked:") {
                        f.identity_linked = OauthProvider::from_db_str(slug);
                    }
                }
            }
        }
        f
    }
}

/// Stage a flash for the next page load. No-op when nothing is set. `domain`
/// must match the session cookie's (empty = host-only); the post-login
/// redirect can cross to another subdomain, so a host-only flash would be
/// dropped where the domain-scoped session survives.
pub fn set(cookies: &Cookies, flash: &Flash, secure: bool, domain: &str) {
    if flash.is_empty() {
        return;
    }
    let mut c = Cookie::new(COOKIE_NAME, flash.encode());
    c.set_http_only(true);
    c.set_secure(secure);
    c.set_same_site(SameSite::Lax);
    c.set_path("/");
    if !domain.is_empty() {
        c.set_domain(domain.to_owned());
    }
    c.set_max_age(Duration::seconds(TTL_SECS));
    cookies.add(c);
}

/// Read and consume the flash: returns the flags and clears the cookie so the
/// banner fires exactly once. `domain` must mirror [`set`] — a removal cookie
/// with a mismatched path/domain leaves the browser's copy in place and the
/// banner repeats.
pub fn take(cookies: &Cookies, domain: &str) -> Flash {
    let Some(c) = cookies.get(COOKIE_NAME) else {
        return Flash::default();
    };
    let flash = Flash::decode(c.value());
    let mut gone = Cookie::new(COOKIE_NAME, "");
    gone.set_path("/");
    if !domain.is_empty() {
        gone.set_domain(domain.to_owned());
    }
    cookies.remove(gone);
    flash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_flags_through_encode_decode() {
        let f = Flash {
            restored: true,
            invite_missed: true,
            ..Default::default()
        };
        let back = Flash::decode(&f.encode());
        assert!(back.restored && back.invite_missed);

        let only = Flash {
            restored: true,
            invite_missed: false,
            ..Default::default()
        };
        let back = Flash::decode(&only.encode());
        assert!(back.restored && !back.invite_missed);
    }

    #[test]
    fn identity_linked_round_trips() {
        let f = Flash {
            identity_linked: Some(OauthProvider::Gitlab),
            ..Default::default()
        };
        assert_eq!(
            Flash::decode(&f.encode()).identity_linked,
            Some(OauthProvider::Gitlab)
        );
        assert_eq!(Flash::default().identity_linked, None);
    }

    #[test]
    fn a_hand_written_provider_never_reaches_the_page() {
        assert_eq!(
            Flash::decode("identity_linked:evilcorp").identity_linked,
            None
        );
        let f = Flash::decode("identity_taken,identity_linked:github");
        assert!(f.identity_taken);
        assert_eq!(f.identity_linked, Some(OauthProvider::Github));
    }

    #[test]
    fn every_flag_survives_set_and_take() {
        // `set` skips a flash it thinks is empty, so a flag missing from
        // `is_empty` is a banner that silently never renders.
        let full = Flash {
            restored: true,
            invite_missed: true,
            identity_taken: true,
            identity_already_linked: true,
            link_failed: true,
            identity_linked: Some(OauthProvider::Google),
        };
        assert!(!full.is_empty());
        let back = Flash::decode(&full.encode());
        assert!(back.restored && back.invite_missed && back.identity_taken);
        assert!(back.identity_already_linked && back.link_failed);
        assert_eq!(back.identity_linked, Some(OauthProvider::Google));

        for one in [
            Flash {
                identity_already_linked: true,
                ..Default::default()
            },
            Flash {
                link_failed: true,
                ..Default::default()
            },
        ] {
            assert!(!one.is_empty(), "a lone flag must still be written");
        }
        assert!(Flash::default().is_empty());
    }

    #[test]
    fn unknown_tags_are_ignored() {
        let f = Flash::decode("restored,bogus,");
        assert!(f.restored && !f.invite_missed);
    }
}
