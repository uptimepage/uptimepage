//! The in-memory [`IncidentStore`], for tests and for running without a
//! database.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::domain::{CheckStatus, OrgId};
use crate::error::Result;

use super::{IncidentStore, NewOpenIncident, OpenIncident};

// ── In-memory implementation (for tests) ────────────────────────────────────

#[derive(Default)]
pub struct InMemoryIncidentStore {
    inner: parking_lot::Mutex<InMemoryIncidentState>,
}

#[derive(Default)]
struct InMemoryIncidentState {
    by_target: std::collections::HashMap<Uuid, Vec<MemIncident>>,
    inserts: u64,
    closes: u64,
    widens: u64,
}

#[derive(Debug, Clone)]
pub struct MemIncident {
    pub id: Uuid,
    pub target_id: Uuid,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub status_at_start: CheckStatus,
    pub check_count: u32,
    pub error_sample: Option<String>,
    pub region: Option<String>,
    pub regions_down: Vec<String>,
    pub regions_up: Vec<String>,
}

impl InMemoryIncidentStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn all_for(&self, target_id: Uuid) -> Vec<MemIncident> {
        self.inner
            .lock()
            .by_target
            .get(&target_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn insert_count(&self) -> u64 {
        self.inner.lock().inserts
    }

    pub fn close_count(&self) -> u64 {
        self.inner.lock().closes
    }

    pub fn widen_count(&self) -> u64 {
        self.inner.lock().widens
    }
}

#[async_trait]
impl IncidentStore for InMemoryIncidentStore {
    async fn open_for_pairs(
        &self,
        pairs: &[(OrgId, Uuid)],
    ) -> Result<std::collections::HashMap<(OrgId, Uuid), Vec<OpenIncident>>> {
        let g = self.inner.lock();
        let mut out = std::collections::HashMap::with_capacity(pairs.len());
        for (org, tid) in pairs {
            let Some(rows) = g.by_target.get(tid) else {
                continue;
            };
            let open: Vec<OpenIncident> = rows
                .iter()
                .filter(|i| i.ended_at.is_none())
                .map(|i| OpenIncident {
                    id: i.id,
                    target_id: i.target_id,
                    started_at: i.started_at,
                    region: i.region.clone(),
                    regions_down: i.regions_down.clone(),
                })
                .collect();
            if !open.is_empty() {
                out.insert((*org, *tid), open);
            }
        }
        Ok(out)
    }

    async fn open_for_target(&self, _org: OrgId, target_id: Uuid) -> Result<Option<OpenIncident>> {
        let g = self.inner.lock();
        let Some(rows) = g.by_target.get(&target_id) else {
            return Ok(None);
        };
        let open = rows
            .iter()
            .filter(|i| i.ended_at.is_none())
            .max_by_key(|i| i.started_at)
            .map(|i| OpenIncident {
                id: i.id,
                target_id: i.target_id,
                started_at: i.started_at,
                region: i.region.clone(),
                regions_down: i.regions_down.clone(),
            });
        Ok(open)
    }

    async fn insert_open(&self, _org: OrgId, new: NewOpenIncident) -> Result<Option<Uuid>> {
        let mut g = self.inner.lock();
        let bucket = g.by_target.entry(new.target_id).or_default();
        // Mirrors the DB unique index: a target already holding an open
        // incident yields None so the racer never pages.
        if bucket.iter().any(|i| i.ended_at.is_none()) {
            return Ok(None);
        }
        let id = Uuid::now_v7();
        bucket.push(MemIncident {
            id,
            target_id: new.target_id,
            started_at: new.started_at,
            ended_at: None,
            status_at_start: new.status_at_start,
            check_count: new.check_count,
            error_sample: new.error_sample,
            region: new.region,
            regions_down: new.regions_down,
            regions_up: new.regions_up,
        });
        g.inserts += 1;
        Ok(Some(id))
    }

    async fn close(&self, _org: OrgId, incident_id: Uuid, ended_at: DateTime<Utc>) -> Result<bool> {
        let mut g = self.inner.lock();
        let mut closed = false;
        for bucket in g.by_target.values_mut() {
            for inc in bucket.iter_mut() {
                if inc.id == incident_id && inc.ended_at.is_none() {
                    inc.ended_at = Some(ended_at);
                    closed = true;
                }
            }
        }
        if closed {
            g.closes += 1;
        }
        Ok(closed)
    }

    async fn widen(&self, _org: OrgId, incident_id: Uuid, regions: &[String]) -> Result<()> {
        let mut g = self.inner.lock();
        for inc in g.by_target.values_mut().flatten() {
            if inc.id == incident_id && inc.ended_at.is_none() {
                for r in regions {
                    if !inc.regions_down.contains(r) {
                        inc.regions_down.push(r.clone());
                    }
                }
                inc.regions_up.retain(|r| !regions.contains(r));
            }
        }
        g.widens += 1;
        Ok(())
    }
}
