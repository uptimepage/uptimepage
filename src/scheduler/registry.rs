use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::{OrgId, Target};
use crate::error::Result;
use crate::storage::admin::EnabledTargetSource;
use crate::worker::host_for_spec;
use crate::worker::host_throttle::{HostKey, HostThrottle};

/// A scheduled target plus its owning tenant. The `org_id` rides with the
/// target through the scheduler→worker→alert path so channel resolution is
/// tenant-scoped. host_key / breaker_key / rdap_tld are pre-computed at
/// registry refresh so dispatch never allocates a host string.
#[derive(Debug, Clone)]
pub struct ScheduledTarget {
    pub org_id: OrgId,
    pub target: Arc<Target>,
    pub host_key: Option<HostKey>,
    pub breaker_key: Arc<str>,
    pub rdap_tld: Option<Arc<str>>,
}

impl ScheduledTarget {
    pub fn build(org_id: OrgId, target: Target) -> Self {
        let target = Arc::new(target);
        let host_key = HostThrottle::key_for(org_id, &target.check);
        let breaker_key: Arc<str> = Arc::from(host_for_spec(&target.check));
        let rdap_tld = match &target.check {
            crate::domain::CheckSpec::DomainExpiry(d) => {
                HostThrottle::rdap_tld(&d.domain).map(Arc::from)
            }
            _ => None,
        };
        Self {
            org_id,
            target,
            host_key,
            breaker_key,
            rdap_tld,
        }
    }
}

pub struct TargetRegistry {
    source: Arc<dyn EnabledTargetSource>,
    targets: DashMap<Uuid, ScheduledTarget>,
}

#[derive(Debug, Default, Clone)]
pub struct RegistryDiff {
    pub added: Vec<ScheduledTarget>,
    pub updated: Vec<ScheduledTarget>,
    pub removed: Vec<Uuid>,
}

impl RegistryDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }
}

impl TargetRegistry {
    pub fn new(source: Arc<dyn EnabledTargetSource>) -> Self {
        Self {
            source,
            targets: DashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.targets.len()
    }

    pub fn get(&self, id: &Uuid) -> Option<ScheduledTarget> {
        self.targets.get(id).map(|e| e.clone())
    }

    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    pub async fn refresh(&self) -> Result<RegistryDiff> {
        let fresh = self.source.list_all_enabled_targets().await?;
        let mut diff = RegistryDiff::default();
        let mut seen = std::collections::HashSet::with_capacity(fresh.len());

        for (org_id, target) in fresh {
            seen.insert(target.id);
            let id = target.id;
            let st = ScheduledTarget::build(org_id, target);
            match self.targets.get(&id) {
                Some(existing) => {
                    // The plan floor is applied to the handed-out copy and
                    // writes nothing back, so a tier change moves the interval
                    // while `updated_at` stands still.
                    if existing.target.updated_at != st.target.updated_at
                        || existing.target.interval != st.target.interval
                    {
                        drop(existing);
                        self.targets.insert(id, st.clone());
                        diff.updated.push(st);
                    }
                }
                None => {
                    self.targets.insert(id, st.clone());
                    diff.added.push(st);
                }
            }
        }

        let removed: Vec<Uuid> = self
            .targets
            .iter()
            .filter_map(|e| (!seen.contains(e.key())).then_some(*e.key()))
            .collect();
        for id in &removed {
            self.targets.remove(id);
        }
        diff.removed = removed;

        Ok(diff)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration as StdDuration;

    use async_trait::async_trait;
    use chrono::Utc;
    use uuid::Uuid;

    use super::*;
    use crate::domain::{
        CheckSpec, ExpectedStatus, HttpCheck, HttpMethod, OrgId, Target, TargetAlerts, WriteSource,
    };

    /// Hands out whatever interval the test last set, on an unchanging row.
    struct FixedSource {
        target: Mutex<Target>,
    }

    #[async_trait]
    impl EnabledTargetSource for FixedSource {
        async fn list_all_enabled_targets(&self) -> Result<Vec<(OrgId, Target)>> {
            Ok(vec![(
                OrgId(Uuid::nil()),
                self.target.lock().expect("lock").clone(),
            )])
        }
    }

    fn a_target(interval: StdDuration) -> Target {
        Target {
            id: Uuid::now_v7(),
            name: "api".into(),
            check: CheckSpec::Http(HttpCheck {
                url: url::Url::parse("https://example.com/").expect("url"),
                method: HttpMethod::Get,
                timeout: StdDuration::from_secs(5),
                follow_redirects: false,
                max_redirects: 0,
                expected_status: ExpectedStatus::Exact(200),
                expected_body_contains: None,
                headers: HashMap::new(),
                body: None,
                verify_tls: true,
                basic_auth: None,
                bearer_token: None,
            }),
            interval,
            enabled: true,
            tags: vec![],
            alerts: TargetAlerts(vec![]),
            alert_confirmations: 1,
            notify_recovery: true,
            renotify_interval_secs: 3600,
            recovery_period_secs: 0,
            region_policy: Default::default(),
            group_name: None,
            owner_user_id: None,
            write_source: WriteSource::Ui,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            plan_hold_at: None,
        }
    }

    #[tokio::test]
    async fn a_clamped_interval_reaches_a_monitor_already_resident() {
        // The plan floor is applied to the handed-out copy and writes nothing
        // back, so a downgrade moves the interval on a row whose `updated_at`
        // never changes. Keying the diff on `updated_at` alone dropped it and
        // the monitor kept its old rate until the process restarted.
        let target = a_target(StdDuration::from_secs(30));
        let id = target.id;
        let source = Arc::new(FixedSource {
            target: Mutex::new(target),
        });
        let registry = TargetRegistry::new(source.clone());

        let first = registry.refresh().await.expect("first refresh");
        assert_eq!(first.added.len(), 1);
        assert_eq!(
            registry.get(&id).expect("resident").target.interval,
            StdDuration::from_secs(30)
        );

        source.target.lock().expect("lock").interval = StdDuration::from_secs(180);

        let second = registry.refresh().await.expect("second refresh");
        assert_eq!(
            second.updated.len(),
            1,
            "a slowed monitor must be handed to the scheduler again"
        );
        assert_eq!(
            registry.get(&id).expect("resident").target.interval,
            StdDuration::from_secs(180)
        );
    }

    #[tokio::test]
    async fn an_unchanged_monitor_is_not_rescheduled() {
        let source = Arc::new(FixedSource {
            target: Mutex::new(a_target(StdDuration::from_secs(30))),
        });
        let registry = TargetRegistry::new(source);

        registry.refresh().await.expect("first refresh");
        let second = registry.refresh().await.expect("second refresh");

        assert!(second.is_empty(), "a steady set must produce no diff");
    }
}
