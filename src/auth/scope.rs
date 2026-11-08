//! API-token permission scopes.
//!
//! A scope is a `resource:action` capability (e.g. `targets:write`) stored in
//! the `api_tokens.scopes` JSONB column and checked per request by the
//! `Authorized<R>` extractor. `write` implies `read` for the same resource;
//! `full_access` is a superset of everything. Sessions are not scoped — only
//! API tokens carry a [`ScopeSet`].

use std::collections::HashSet;

/// One permission. Stored/serialized as its `as_str()` form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    TargetsRead,
    TargetsWrite,
    /// Delete targets — split from `write` so a config token can't destroy.
    TargetsDelete,
    /// Trigger checks / probes (`check-now`, ad-hoc test) — a side effect, not
    /// a config write.
    TargetsExecute,
    ChannelsRead,
    ChannelsWrite,
    ChannelsDelete,
    /// Send a test notification through a channel.
    ChannelsExecute,
    IncidentsRead,
    IncidentsWrite,
    /// On-call / escalation configuration (policies, schedules) — org config,
    /// not incident data, so it carries its own resource.
    OnCallRead,
    OnCallWrite,
    MaintenanceRead,
    MaintenanceWrite,
    MaintenanceDelete,
    StatusPageRead,
    StatusPageWrite,
    StatusPageDelete,
    /// Reusable org variables + the secret credential store.
    VariablesRead,
    VariablesWrite,
    /// The tools that send alerts in. `write` can read their ingest URLs.
    AlertSourcesRead,
    AlertSourcesWrite,
    AlertSourcesDelete,
    /// Superset of every other scope.
    FullAccess,
}

impl Scope {
    /// Canonical wire/storage string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Scope::TargetsRead => "targets:read",
            Scope::TargetsWrite => "targets:write",
            Scope::TargetsDelete => "targets:delete",
            Scope::TargetsExecute => "targets:execute",
            Scope::ChannelsRead => "channels:read",
            Scope::ChannelsWrite => "channels:write",
            Scope::ChannelsDelete => "channels:delete",
            Scope::ChannelsExecute => "channels:execute",
            Scope::IncidentsRead => "incidents:read",
            Scope::IncidentsWrite => "incidents:write",
            Scope::OnCallRead => "oncall:read",
            Scope::OnCallWrite => "oncall:write",
            Scope::MaintenanceRead => "maintenance:read",
            Scope::MaintenanceWrite => "maintenance:write",
            Scope::MaintenanceDelete => "maintenance:delete",
            Scope::StatusPageRead => "status_page:read",
            Scope::StatusPageWrite => "status_page:write",
            Scope::StatusPageDelete => "status_page:delete",
            Scope::VariablesRead => "variables:read",
            Scope::VariablesWrite => "variables:write",
            Scope::AlertSourcesRead => "alert_sources:read",
            Scope::AlertSourcesWrite => "alert_sources:write",
            Scope::AlertSourcesDelete => "alert_sources:delete",
            Scope::FullAccess => "full_access",
        }
    }

    /// Parse a stored/submitted string. Unknown strings yield `None`.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "targets:read" => Scope::TargetsRead,
            "targets:write" => Scope::TargetsWrite,
            "targets:delete" => Scope::TargetsDelete,
            "targets:execute" => Scope::TargetsExecute,
            "channels:read" => Scope::ChannelsRead,
            "channels:write" => Scope::ChannelsWrite,
            "channels:delete" => Scope::ChannelsDelete,
            "channels:execute" => Scope::ChannelsExecute,
            "incidents:read" => Scope::IncidentsRead,
            "incidents:write" => Scope::IncidentsWrite,
            "oncall:read" => Scope::OnCallRead,
            "oncall:write" => Scope::OnCallWrite,
            "maintenance:read" => Scope::MaintenanceRead,
            "maintenance:write" => Scope::MaintenanceWrite,
            "maintenance:delete" => Scope::MaintenanceDelete,
            "status_page:read" => Scope::StatusPageRead,
            "status_page:write" => Scope::StatusPageWrite,
            "status_page:delete" => Scope::StatusPageDelete,
            "variables:read" => Scope::VariablesRead,
            "variables:write" => Scope::VariablesWrite,
            "alert_sources:read" => Scope::AlertSourcesRead,
            "alert_sources:write" => Scope::AlertSourcesWrite,
            "alert_sources:delete" => Scope::AlertSourcesDelete,
            "full_access" => Scope::FullAccess,
            _ => return None,
        })
    }

    /// The write scope that subsumes this read scope (`targets:read` →
    /// `targets:write`). Returns `None` for write scopes and `full_access`.
    const fn implied_by_write(self) -> Option<Scope> {
        Some(match self {
            Scope::TargetsRead => Scope::TargetsWrite,
            Scope::ChannelsRead => Scope::ChannelsWrite,
            Scope::IncidentsRead => Scope::IncidentsWrite,
            Scope::OnCallRead => Scope::OnCallWrite,
            Scope::MaintenanceRead => Scope::MaintenanceWrite,
            Scope::StatusPageRead => Scope::StatusPageWrite,
            Scope::VariablesRead => Scope::VariablesWrite,
            Scope::AlertSourcesRead => Scope::AlertSourcesWrite,
            _ => return None,
        })
    }
}

/// The set of scopes a token carries. Empty means no capability (denies all);
/// in practice tokens always carry at least one scope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeSet(HashSet<Scope>);

impl ScopeSet {
    /// The historical default — full access to everything.
    pub fn full_access() -> Self {
        ScopeSet(HashSet::from([Scope::FullAccess]))
    }

    /// Build from stored/submitted strings, silently dropping unknown entries.
    /// Forward-compatible: a token written by a newer version is downgraded to
    /// the scopes this version understands rather than failing the request.
    pub fn from_strs<'a>(it: impl IntoIterator<Item = &'a str>) -> Self {
        ScopeSet(it.into_iter().filter_map(Scope::parse).collect())
    }

    /// Does this set grant `required`? `full_access` grants everything; an
    /// exact match grants it; and a `*:write` scope grants the matching
    /// `*:read`.
    pub fn allows(&self, required: Scope) -> bool {
        self.0.contains(&Scope::FullAccess)
            || self.0.contains(&required)
            || required
                .implied_by_write()
                .is_some_and(|w| self.0.contains(&w))
    }

    /// Canonical, de-duplicated string forms (sorted for stable output).
    pub fn to_strings(&self) -> Vec<String> {
        let mut v: Vec<String> = self.0.iter().map(|s| s.as_str().to_owned()).collect();
        v.sort();
        v
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_access_allows_everything() {
        let s = ScopeSet::full_access();
        assert!(s.allows(Scope::TargetsWrite));
        assert!(s.allows(Scope::StatusPageRead));
    }

    #[test]
    fn write_implies_read_same_resource_only() {
        let s = ScopeSet::from_strs(["targets:write"]);
        assert!(s.allows(Scope::TargetsWrite));
        assert!(s.allows(Scope::TargetsRead)); // write ⇒ read
        assert!(!s.allows(Scope::ChannelsRead)); // not cross-resource
        assert!(!s.allows(Scope::ChannelsWrite));
    }

    #[test]
    fn read_only_denies_write() {
        let s = ScopeSet::from_strs(["targets:read", "channels:read"]);
        assert!(s.allows(Scope::TargetsRead));
        assert!(!s.allows(Scope::TargetsWrite));
        assert!(s.allows(Scope::ChannelsRead));
    }

    #[test]
    fn unknown_scopes_dropped_round_trip() {
        let s = ScopeSet::from_strs(["targets:read", "future:scope", "garbage"]);
        assert_eq!(s.to_strings(), vec!["targets:read"]);
        assert!(!s.allows(Scope::TargetsWrite));
    }

    #[test]
    fn parse_as_str_round_trip() {
        for raw in [
            "targets:read",
            "targets:write",
            "targets:delete",
            "targets:execute",
            "channels:read",
            "channels:write",
            "channels:delete",
            "channels:execute",
            "incidents:read",
            "incidents:write",
            "oncall:read",
            "oncall:write",
            "maintenance:read",
            "maintenance:write",
            "maintenance:delete",
            "status_page:read",
            "status_page:write",
            "status_page:delete",
            "variables:read",
            "variables:write",
            "alert_sources:read",
            "alert_sources:write",
            "alert_sources:delete",
            "full_access",
        ] {
            assert_eq!(Scope::parse(raw).unwrap().as_str(), raw);
        }
        assert!(Scope::parse("nope").is_none());
    }
}
