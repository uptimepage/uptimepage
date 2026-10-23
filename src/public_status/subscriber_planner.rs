//! One planner pass: decides what every page's subscribers should hear and
//! writes it down as notices. Incident updates and maintenance events become
//! notices of their own; monitor incidents that fail together on a page are
//! reconciled into outages, which are told once when they start and once when
//! they end. Runs on one instance at a time. It reads one snapshot, then
//! writes each page in a transaction of its own: a page is planned whole or
//! not at all, and one page's trouble never holds up the others.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Context;
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::Locale;
use crate::public_status::outages::{self, Candidate, Member, MemberFacts, Outage};
use crate::public_status::subscriber_notice::{
    ComponentSnapshot, NoticeBody, NoticePayload, PageLinks,
};
use crate::storage::page_sql::PageRow;
use crate::storage::subscriber_maintenance;
use crate::storage::subscriber_notices::{
    self, EMAIL, INCIDENT_UPDATE, MAINTENANCE, MAINTENANCE_UPDATE, NewNotice, OUTAGE_RESOLVED,
    OUTAGE_STARTED, WEBHOOK,
};
use crate::storage::subscriber_outages::{self, CandidateRow, MemberWrite, OutageWrite};
use crate::storage::subscribers::{self, FANOUT_LOOKBACK_HOURS};

/// Sources read per kind per pass; anything past it waits for the next pass.
const PASS_LIMIT: i64 = 1000;

pub async fn plan(pool: &PgPool, links: &PageLinks) -> anyhow::Result<()> {
    let mut tx = pool.begin().await.context("planner begin")?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()")
        .fetch_one(&mut *tx)
        .await
        .context("planner clock")?;

    let mut notices = Vec::new();
    for row in subscribers::pending_update_notices(&mut tx, PASS_LIMIT).await? {
        notices.push(NewNotice {
            org_id: row.page.org_id,
            page_id: row.page.page_id,
            kind: INCIDENT_UPDATE,
            source_ref: row.update_id,
            phase: String::new(),
            channels: if row.email_too() {
                vec![EMAIL, WEBHOOK]
            } else {
                vec![WEBHOOK]
            },
            payload: payload(
                links,
                &row.page,
                NoticeBody::IncidentUpdate {
                    incident_id: row.incident_id,
                    incident_title: row.incident_title(),
                    update_id: row.update_id,
                    phase: row.phase.clone(),
                    message: row.message.clone(),
                },
            ),
        });
    }
    for row in subscriber_maintenance::pending_window_notices(&mut tx, PASS_LIMIT).await? {
        let description = row.description.filter(|d| !d.trim().is_empty());
        notices.push(NewNotice {
            org_id: row.page.org_id,
            page_id: row.page.page_id,
            kind: MAINTENANCE,
            source_ref: row.maintenance_id,
            phase: row.phase.clone(),
            channels: vec![EMAIL, WEBHOOK],
            payload: payload(
                links,
                &row.page,
                NoticeBody::Maintenance {
                    maintenance_id: row.maintenance_id,
                    title: row.title,
                    description,
                    phase: row.phase,
                    starts_at: row.starts_at,
                    ends_at: row.ends_at,
                },
            ),
        });
    }
    for row in subscriber_maintenance::pending_window_update_notices(&mut tx, PASS_LIMIT).await? {
        notices.push(NewNotice {
            org_id: row.page.org_id,
            page_id: row.page.page_id,
            kind: MAINTENANCE_UPDATE,
            source_ref: row.update_id,
            phase: String::new(),
            channels: vec![EMAIL, WEBHOOK],
            payload: payload(
                links,
                &row.page,
                NoticeBody::MaintenanceUpdate {
                    maintenance_id: row.maintenance_id,
                    title: row.title,
                    starts_at: row.starts_at,
                    ends_at: row.ends_at,
                    update_id: row.update_id,
                    phase: row.phase,
                    message: row.message,
                },
            ),
        });
    }

    let mut pages = gather(
        subscriber_outages::open_members(&mut tx).await?,
        subscriber_outages::candidates(&mut tx).await?,
    );
    tx.commit().await.context("planner read")?;
    subscriber_notices::insert(&mut *pool.acquire().await?, &notices).await?;

    // Each page in a transaction of its own, so one page's bad data holds up
    // only that page; the rest of the pass still lands and it is retried next
    // pass.
    let mut failed = None;
    for page in pages.values_mut() {
        let mut outage_notices = Vec::new();
        page.reconcile(now, links, &mut outage_notices);
        if let Err(err) = page.write(pool, &outage_notices).await {
            tracing::warn!(error = %err, page = %page.page.page_id, "planner: page not planned");
            failed.get_or_insert(err);
        }
    }
    failed.map_or(Ok(()), Err)
}

fn payload(links: &PageLinks, page: &PageRow, body: NoticeBody) -> serde_json::Value {
    let payload = NoticePayload {
        page: links.snapshot(page),
        body,
    };
    serde_json::to_value(payload).expect("a notice payload is plain data")
}

/// What a mail says about one member's incident.
#[derive(Debug, Clone)]
struct Shown {
    name: String,
    title: String,
    degraded: bool,
    /// Public now; an incident taken off the page is never announced.
    public: bool,
}

impl Shown {
    fn new(
        locale: Locale,
        name: &str,
        public_title: Option<&str>,
        status_at_start: &str,
        public: bool,
    ) -> Self {
        Self {
            name: name.to_owned(),
            title: subscribers::incident_title(locale, public_title, name, status_at_start),
            degraded: status_at_start == "degraded",
            public,
        }
    }
}

/// One outage being planned and what changed in it.
struct Planned {
    outage: Outage,
    id: Uuid,
    /// Its started mail went out, or its start was told some other way.
    started_told: bool,
    resolved_at: Option<DateTime<Utc>>,
    dirty: bool,
    /// Members this pass added or changed.
    touched: Vec<Uuid>,
}

/// Everything one page's pass needs.
struct PagePlan {
    page: PageRow,
    planned: Vec<Planned>,
    /// Members' incident facts, by incident.
    facts: HashMap<Uuid, MemberFacts>,
    shown: HashMap<Uuid, Shown>,
    candidates: Vec<CandidateRow>,
}

fn gather(
    members: Vec<subscriber_outages::MemberRow>,
    candidates: Vec<CandidateRow>,
) -> BTreeMap<Uuid, PagePlan> {
    let mut pages: BTreeMap<Uuid, PagePlan> = BTreeMap::new();
    for row in members {
        let plan = PagePlan::of(&mut pages, &row.page);
        plan.note(
            row.incident_id,
            &row.component_name,
            row.public_title.as_deref(),
            &row.status_at_start,
            row.public,
        );
        plan.facts.insert(
            row.incident_id,
            MemberFacts {
                ended_at: row.incident_ended_at,
                resolved_at: row.resolved_at,
                shows: row.shows,
            },
        );
        let member = Member {
            incident_id: row.incident_id,
            target_id: row.target_id,
            started_at: row.started_at,
            ended_at: row.ended_at,
            closed_at: row.closed_at,
        };
        match plan.planned.iter_mut().find(|p| p.id == row.outage_id) {
            Some(p) => p.outage.members.push(member),
            None => plan.planned.push(Planned {
                outage: Outage {
                    opened_at: row.opened_at,
                    started_at: row.outage_started_at,
                    recovered_at: row.recovered_at,
                    members: vec![member],
                },
                id: row.outage_id,
                started_told: row.started_told,
                resolved_at: None,
                dirty: false,
                touched: Vec::new(),
            }),
        }
    }
    for row in candidates {
        let plan = PagePlan::of(&mut pages, &row.page);
        plan.note(
            row.incident_id,
            &row.component_name,
            row.public_title.as_deref(),
            &row.status_at_start,
            row.public,
        );
        plan.candidates.push(row);
    }
    pages
}

impl PagePlan {
    fn of<'a>(pages: &'a mut BTreeMap<Uuid, PagePlan>, page: &PageRow) -> &'a mut PagePlan {
        pages.entry(page.page_id).or_insert_with(|| PagePlan {
            page: page.clone(),
            planned: Vec::new(),
            facts: HashMap::new(),
            shown: HashMap::new(),
            candidates: Vec::new(),
        })
    }

    fn note(
        &mut self,
        incident: Uuid,
        name: &str,
        public_title: Option<&str>,
        status_at_start: &str,
        public: bool,
    ) {
        let locale = Locale::from_db(&self.page.public_locale);
        self.shown.insert(
            incident,
            Shown::new(locale, name, public_title, status_at_start, public),
        );
    }

    fn reconcile(&mut self, now: DateTime<Utc>, links: &PageLinks, notices: &mut Vec<NewNotice>) {
        // Close and reopen members against what their incidents say now.
        for p in &mut self.planned {
            let mut changed = false;
            for m in &mut p.outage.members {
                let facts = self.facts[&m.incident_id];
                if outages::reconcile(m, facts, now) {
                    p.touched.push(m.incident_id);
                    changed = true;
                }
            }
            if changed {
                p.outage.refresh();
                p.dirty = true;
            }
        }

        // Join the incidents no outage holds yet.
        let told: HashSet<Uuid> = self
            .candidates
            .iter()
            .filter(|c| c.told_before)
            .map(|c| c.incident_id)
            .collect();
        let candidates: Vec<Candidate> = std::mem::take(&mut self.candidates)
            .into_iter()
            .map(|c| Candidate {
                member: Member {
                    incident_id: c.incident_id,
                    target_id: c.target_id,
                    started_at: c.started_at,
                    ended_at: c.ended_at,
                    closed_at: c.ended_at.map(|ended| c.resolved_at.unwrap_or(ended)),
                },
                said_at: c.said_at,
            })
            .collect();
        if !candidates.is_empty() {
            let known = self.planned.len();
            let mut list: Vec<Outage> = self.planned.iter().map(|p| p.outage.clone()).collect();
            for (incident, at) in outages::join(&mut list, candidates) {
                // `join` appends new outages in the order it opens them.
                if at >= self.planned.len() {
                    self.planned.push(Planned {
                        outage: list[at].clone(),
                        id: Uuid::now_v7(),
                        started_told: false,
                        resolved_at: None,
                        dirty: true,
                        touched: Vec::new(),
                    });
                }
                let p = &mut self.planned[at];
                p.outage = list[at].clone();
                p.dirty = true;
                p.touched.push(incident);
            }
            // An incident that left the page while down and came back is not
            // news: an outage made only of such incidents was told already.
            for p in &mut self.planned[known..] {
                p.started_told = p
                    .outage
                    .members
                    .iter()
                    .all(|m| told.contains(&m.incident_id));
            }
        }

        let lookback = Duration::hours(FANOUT_LOOKBACK_HOURS);
        let Self {
            page,
            planned,
            shown,
            ..
        } = self;
        for p in planned.iter_mut() {
            // Still gathering: neither told nor ended yet, so it is never
            // stored as ended before anyone heard of it.
            if p.outage.gathering(now) {
                continue;
            }
            if p.outage.resolvable(now) {
                p.resolved_at = Some(now);
                p.dirty = true;
            } else if !p.started_told && p.outage.starts(now, lookback) {
                // Nothing public to name yet: looked at again next pass.
                if let Some(started) = outage_notice(links, page, shown, p, false) {
                    notices.push(started);
                    p.started_told = true;
                    p.dirty = true;
                }
            }
            if p.resolved_at.is_some() {
                notices.extend(outage_notice(links, page, shown, p, true));
            }
        }
    }

    async fn write(&self, pool: &PgPool, notices: &[NewNotice]) -> anyhow::Result<()> {
        let mut outages = Vec::new();
        let mut members = Vec::new();
        self.writes(&mut outages, &mut members);
        if outages.is_empty() && notices.is_empty() {
            return Ok(());
        }
        let mut tx = pool.begin().await.context("planner page begin")?;
        subscriber_outages::write(&mut tx, &outages, &members).await?;
        subscriber_notices::insert(&mut tx, notices).await?;
        tx.commit().await.context("planner page commit")?;
        Ok(())
    }

    fn writes(&self, outages: &mut Vec<OutageWrite>, members: &mut Vec<MemberWrite>) {
        for p in self.planned.iter().filter(|p| p.dirty) {
            outages.push(OutageWrite {
                id: p.id,
                org_id: self.page.org_id,
                page_id: self.page.page_id,
                opened_at: p.outage.opened_at,
                started_at: p.outage.started_at,
                recovered_at: p.outage.recovered_at,
                resolved_at: p.resolved_at,
                started_told: p.started_told,
            });
            for m in p
                .outage
                .members
                .iter()
                .filter(|m| p.touched.contains(&m.incident_id))
            {
                members.push(MemberWrite {
                    outage_id: p.id,
                    org_id: self.page.org_id,
                    page_id: self.page.page_id,
                    incident_id: m.incident_id,
                    target_id: m.target_id,
                    component_name: self.shown[&m.incident_id].name.clone(),
                    ended_at: m.ended_at,
                    closed_at: m.closed_at,
                });
            }
        }
    }
}

/// The outage's notice, one row per monitor: a monitor that flapped back
/// into it is one component, from its first failure to its last recovery.
/// The started mail lists what is still down and still public; the resolved
/// mail lists what recovered, leaving out a component the page stopped
/// showing while it was down. Neither is sent with nothing to list.
fn outage_notice(
    links: &PageLinks,
    page: &PageRow,
    shown: &HashMap<Uuid, Shown>,
    p: &Planned,
    resolved: bool,
) -> Option<NewNotice> {
    let mut components: Vec<(Uuid, ComponentSnapshot)> = Vec::new();
    let mut members: Vec<&Member> = p
        .outage
        .members
        .iter()
        .filter(|m| {
            if resolved {
                m.ended_at.is_some()
            } else {
                m.closed_at.is_none() && shown[&m.incident_id].public
            }
        })
        .collect();
    members.sort_by_key(|m| m.started_at);
    for m in members {
        let shown = &shown[&m.incident_id];
        match components
            .iter_mut()
            .find(|(target, _)| *target == m.target_id)
        {
            Some((_, c)) => {
                c.title = shown.title.clone();
                c.incident_id = m.incident_id;
                c.degraded &= shown.degraded;
                c.ended_at = c.ended_at.max(m.ended_at);
            }
            None => components.push((
                m.target_id,
                ComponentSnapshot {
                    name: shown.name.clone(),
                    title: shown.title.clone(),
                    incident_id: m.incident_id,
                    degraded: shown.degraded,
                    started_at: m.started_at,
                    ended_at: m.ended_at,
                },
            )),
        }
    }
    if components.is_empty() {
        return None;
    }
    Some(NewNotice {
        org_id: page.org_id,
        page_id: page.page_id,
        kind: if resolved {
            OUTAGE_RESOLVED
        } else {
            OUTAGE_STARTED
        },
        source_ref: p.id,
        phase: String::new(),
        channels: vec![EMAIL],
        payload: payload(
            links,
            page,
            NoticeBody::Outage {
                resolved,
                components: components.into_iter().map(|(_, c)| c).collect(),
            },
        ),
    })
}
