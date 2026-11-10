//! What the form says under the domain expiry input: the registration the
//! check queries when it is not the name typed, and the monitor that already
//! watches it.

use std::net::IpAddr;

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Query, State};
use serde::Deserialize;
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{
    CheckSpec, OrgId, Target, is_monitorable, reduced_domain_hint, registered_domain,
};
use crate::net::host::canonical_host_strict;
use crate::request::{AuthedBrowser, CurrentOrg};
use crate::storage::traits::{TargetFilter, TargetSort};
use crate::web::error::WebResult;

/// The store's ceiling: its default page of 100 could hide a match.
const ALL_MONITORS: usize = 10_000;

#[derive(Default)]
pub struct DomainNote {
    pub queried: Option<String>,
    pub watcher: Option<DomainWatcher>,
}

impl DomainNote {
    pub fn is_empty(&self) -> bool {
        self.queried.is_none() && self.watcher.is_none()
    }
}

pub struct DomainWatcher {
    pub id: Uuid,
    pub name: String,
    pub registered_domain: String,
}

/// `editing` is the monitor the form is open on, which must not match itself.
pub(super) async fn domain_note(
    state: &AppState,
    org: OrgId,
    typed: &str,
    editing: Option<Uuid>,
) -> WebResult<DomainNote> {
    let Some(name) = lookup_name(typed) else {
        return Ok(DomainNote::default());
    };
    let monitors = state
        .target_store
        .list(
            org,
            TargetFilter {
                kind: Some("domain_expiry".into()),
                limit: Some(ALL_MONITORS),
                // Oldest first, so the monitor named does not change when one is edited.
                sort: TargetSort::Created,
                ..TargetFilter::default()
            },
        )
        .await?;
    Ok(DomainNote {
        queried: reduced_domain_hint(&name),
        watcher: watcher(&name, monitors, editing),
    })
}

/// The name as a save would store it, or `None` when a save would refuse it
/// or no registry could be asked about it: a pasted URL or `host:port`, an
/// address, a suffix that is not one or whose registry publishes no expiry.
/// Offering to use what such text reduces to would put a broken name in the
/// field.
fn lookup_name(typed: &str) -> Option<String> {
    let typed = typed.trim();
    if typed.is_empty() || typed.parse::<IpAddr>().is_ok() {
        return None;
    }
    let name = canonical_host_strict(typed).ok()?;
    let known_suffix = psl::suffix(name.as_bytes()).is_some_and(|s| s.is_known());
    let tld = name.rsplit('.').next()?;
    (known_suffix && is_monitorable(tld)).then_some(name)
}

/// The first of `monitors` on the same registration as `name`. The
/// registration alone decides: a second monitor may differ in thresholds,
/// interval or channels and still spend quota on the same fact.
fn watcher(name: &str, monitors: Vec<Target>, editing: Option<Uuid>) -> Option<DomainWatcher> {
    let registered = registered_domain(name);
    monitors.into_iter().find_map(|t| {
        let same = matches!(
            &t.check,
            CheckSpec::DomainExpiry(d) if registered_domain(&d.domain) == registered
        );
        (same && Some(t.id) != editing).then(|| DomainWatcher {
            id: t.id,
            name: t.name,
            registered_domain: registered.clone(),
        })
    })
}

#[derive(Deserialize)]
pub struct NoteParams {
    #[serde(default, rename = "domain_expiry_domain")]
    domain: String,
    editing: Option<Uuid>,
}

#[derive(Template, WebTemplate)]
#[template(path = "targets/partials/_domain_note.html")]
pub struct DomainNotePartial {
    note: DomainNote,
}

pub async fn domain_note_partial(
    _auth: AuthedBrowser,
    CurrentOrg(org): CurrentOrg,
    State(state): State<AppState>,
    Query(params): Query<NoteParams>,
) -> WebResult<DomainNotePartial> {
    let note = domain_note(&state, org, &params.domain, params.editing).await?;
    Ok(DomainNotePartial { note })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::domain::DomainExpiryCheck;

    fn monitor(name: &str, domain: &str) -> Target {
        let mut t = Target::sample(Duration::from_secs(86_400));
        t.id = Uuid::new_v4();
        t.name = name.into();
        t.check = CheckSpec::DomainExpiry(DomainExpiryCheck {
            domain: domain.into(),
            warn_days: 30,
            critical_days: 7,
            timeout: Duration::from_secs(10),
        });
        t
    }

    fn watcher_name(name: &str, monitors: Vec<Target>) -> Option<String> {
        watcher(name, monitors, None).map(|w| w.name)
    }

    #[test]
    fn the_name_is_looked_up_as_it_would_be_stored() {
        assert_eq!(
            lookup_name("  App.Bücher.COM. ").as_deref(),
            Some("app.xn--bcher-kva.com")
        );
    }

    #[test]
    fn text_a_save_would_refuse_is_never_looked_up() {
        for typed in [
            "",
            "   ",
            "https://app.example.com",
            "www.example.com/",
            "app.example.com:8443",
            "not a domain",
            "203.0.113.5",
            "2001:db8::1",
        ] {
            assert_eq!(lookup_name(typed), None, "{typed:?}");
        }
    }

    #[test]
    fn a_name_no_registry_answers_for_is_never_looked_up() {
        // DENIC publishes no expiry, and `.o` is not a suffix at all.
        assert_eq!(lookup_name("www.example.de"), None);
        assert_eq!(lookup_name("app.example.o"), None);
    }

    #[test]
    fn a_subdomain_matches_the_monitor_on_its_registration() {
        let found = watcher(
            "app.example.com",
            vec![monitor("apex", "example.com")],
            None,
        )
        .unwrap();
        assert_eq!(found.name, "apex");
        assert_eq!(found.registered_domain, "example.com");
    }

    #[test]
    fn two_subdomains_of_one_registration_match_each_other() {
        assert_eq!(
            watcher_name(
                "api.example.co.uk",
                vec![monitor("www", "www.example.co.uk")]
            )
            .as_deref(),
            Some("www")
        );
    }

    #[test]
    fn another_registration_under_the_same_suffix_does_not_match() {
        assert_eq!(
            watcher_name("other.co.uk", vec![monitor("apex", "example.co.uk")]),
            None
        );
    }

    #[test]
    fn the_first_monitor_on_the_registration_is_the_one_named() {
        let monitors = vec![
            monitor("elsewhere", "example.org"),
            monitor("older", "www.example.com"),
            monitor("newer", "example.com"),
        ];
        assert_eq!(
            watcher_name("app.example.com", monitors).as_deref(),
            Some("older")
        );
    }

    #[test]
    fn the_monitor_being_edited_does_not_match_itself() {
        let own = monitor("own", "example.com");
        let id = own.id;
        assert!(watcher("example.com", vec![own], Some(id)).is_none());

        let own = monitor("own", "example.com");
        let id = own.id;
        let found = watcher(
            "example.com",
            vec![own, monitor("other", "www.example.com")],
            Some(id),
        );
        assert_eq!(found.map(|w| w.name).as_deref(), Some("other"));
    }

    #[test]
    fn other_kinds_never_match() {
        let mut http = Target::sample(Duration::from_secs(60));
        http.name = "http".into();
        assert_eq!(watcher_name("example.com", vec![http]), None);
    }
}
