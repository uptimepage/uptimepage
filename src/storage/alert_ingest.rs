//! What an alert event does to its source's incidents. One event, one
//! transaction, under the source's advisory lock: that one lock serialises the
//! cap count and every key of the source, and a source delete takes it too.
//!
//! LOCK ORDER: the source's advisory lock, then the incident's, then the
//! incident row (the lifecycle core takes the last two again, which an
//! advisory lock held already allows). Nothing takes them the other way.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::{
    AlertGrouping, AlertSource, IncidentEventKind, IncidentSeverity, IncidentUrgency,
    NotificationReason, OrgId,
};
use crate::error::Result;
use crate::intake::{AlertEvent, EventAction, MAX_DETAIL_CHARS, clean_block};
use crate::storage::db;
use crate::storage::incident_ops::{Actor, LifecycleOutcome, insert_event_tx, resolve_in_tx};
use crate::storage::locks::{advisory_xact_lock, alert_source_lock_key, incident_lock_key};

/// Open incidents a source may hold before new keys go to its summary.
pub const MAX_OPEN_PER_SOURCE: i64 = 50;
/// The summary incident a source at its cap opens instead.
pub const OVERFLOW_KEY: &str = "uptimepage:overflow";
/// The incident a body nobody could read opens.
pub const UNPARSED_KEY: &str = "uptimepage:unparsed";
/// Clock skew allowed between a sender's times and ours.
pub const RESOLVE_SKEW_SECS: i64 = 60;
/// A repeat that changes nothing, and the source's last-event stamp, write at
/// most this often.
const COALESCE_SECS: f64 = 60.0;

/// What one event did, as the sender is told and the delivery log records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Opened,
    Repeat,
    Resolved,
    /// A resolve with nothing open, or one older than what it would close.
    Ignored,
    /// Paused: a firing that would open an incident opens nothing. A key
    /// already open still repeats, and a resolve still closes.
    Paused,
    /// Held by the plan: a firing that would open an incident is refused. A
    /// key already open still repeats, and a resolve still closes.
    Held,
    /// At the cap: counted on the source's summary incident instead.
    Capped,
    /// The body could not be read and went to the unparsed incident.
    Unparsed,
    /// The source was deleted since its token was checked.
    Gone,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Opened => "opened",
            Self::Repeat => "repeat",
            Self::Resolved => "resolved",
            Self::Ignored => "ignored",
            Self::Paused => "paused",
            Self::Held => "held",
            Self::Capped => "capped",
            Self::Unparsed => "unparsed",
            Self::Gone => "gone",
        }
    }
}

/// An event's result: what it did, the incident it touched, and what to tell
/// the engine once the transaction has committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub status: Status,
    pub incident: Option<Uuid>,
    /// Every incident the event opened or closed, the summary included.
    pub signals: Vec<(Uuid, NotificationReason)>,
    /// Why it was ignored, for the delivery log.
    pub note: Option<&'static str>,
}

impl Applied {
    fn just(status: Status) -> Self {
        Self {
            status,
            incident: None,
            signals: Vec::new(),
            note: None,
        }
    }

    fn on(status: Status, incident: Uuid) -> Self {
        Self {
            incident: Some(incident),
            ..Self::just(status)
        }
    }

    fn signalling(mut self, incident: Uuid, reason: NotificationReason) -> Self {
        self.signals.push((incident, reason));
        self
    }

    fn ignored(note: &'static str) -> Self {
        Self {
            note: Some(note),
            ..Self::just(Status::Ignored)
        }
    }
}

/// What applying one event came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ingested {
    Applied(Applied),
    /// Nothing was written: the source's grouping has changed since the
    /// event was parsed, to this one. Parse the body again under it.
    Regrouped(AlertGrouping),
}

impl Ingested {
    /// The event's result, unless it has to be parsed again.
    pub fn applied(self) -> Option<Applied> {
        match self {
            Self::Applied(applied) => Some(applied),
            Self::Regrouped(_) => None,
        }
    }
}

pub struct AlertIngest {
    pool: PgPool,
}

#[derive(sqlx::FromRow)]
struct Open {
    id: Uuid,
    severity: String,
    fired_at: Option<DateTime<Utc>>,
    reported_severity: Option<String>,
}

/// The source as it stands under its lock.
#[derive(sqlx::FromRow)]
struct Live {
    id: Uuid,
    name: String,
    enabled: bool,
    held: bool,
    grouping: String,
}

impl Live {
    /// The grouping now, when `source` was parsed under another one and its
    /// kind reads a body differently under each.
    fn regrouped(&self, source: &AlertSource) -> Option<AlertGrouping> {
        let now = AlertGrouping::from_db(&self.grouping);
        (crate::intake::parses_by_grouping(source.kind) && now != source.grouping).then_some(now)
    }
}

type Tx<'a> = sqlx::Transaction<'a, sqlx::Postgres>;

fn urgency(severity: IncidentSeverity) -> IncidentUrgency {
    match severity {
        IncidentSeverity::Minor => IncidentUrgency::Low,
        _ => IncidentUrgency::High,
    }
}

impl AlertIngest {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Apply one event from `source`, parsed under `source.grouping`.
    pub async fn apply(
        &self,
        org: OrgId,
        source: &AlertSource,
        event: &AlertEvent,
    ) -> Result<Ingested> {
        let mut tx = self.pool.begin().await.map_err(db("ingest: begin"))?;
        let Some(live) = lock_source(&mut tx, org, source.id).await? else {
            return Ok(Ingested::Applied(Applied::just(Status::Gone)));
        };
        // The key was cut under the grouping read before the body arrived.
        // Applied under another, a firing would open an incident its later
        // events never match and a resolve would miss the one they opened.
        if let Some(now) = live.regrouped(source) {
            return Ok(Ingested::Regrouped(now));
        }
        let open = open_locked(&mut tx, org, live.id, &event.key).await?;
        let applied = match (event.action, open) {
            (EventAction::Fire, Some(open)) => repeat(&mut tx, org, &live, open, event).await?,
            (EventAction::Fire, None) if !event.may_open => {
                Applied::ignored("a resolve with alerts cut from it, and nothing open")
            }
            (EventAction::Fire, None) if !live.enabled => Applied::just(Status::Paused),
            (EventAction::Fire, None) if live.held => Applied::just(Status::Held),
            (EventAction::Fire, None) => fire(&mut tx, org, &live, event).await?,
            (EventAction::Resolve, None) => Applied::ignored("nothing open for this key"),
            (EventAction::Resolve, Some(open)) => {
                resolve(&mut tx, org, &live, &event.key, open, event.at).await?
            }
        };
        // A person's resolve frees room without passing through here, so any
        // event that finds the source under its cap closes a summary left open.
        let applied = match applied.status {
            Status::Capped => applied,
            _ => close_summary_if_room(&mut tx, org, &live, applied).await?,
        };
        stamp_event(&mut tx, org, live.id).await?;
        tx.commit().await.map_err(db("ingest: commit"))?;
        Ok(Ingested::Applied(applied))
    }

    /// A body from `source` that could not be read: one incident holds every
    /// such failure until a person resolves it.
    pub async fn apply_unparsed(
        &self,
        org: OrgId,
        source: &AlertSource,
        reason: &str,
    ) -> Result<Ingested> {
        let mut tx = self.pool.begin().await.map_err(db("ingest: begin"))?;
        let Some(live) = lock_source(&mut tx, org, source.id).await? else {
            return Ok(Ingested::Applied(Applied::just(Status::Gone)));
        };
        // Whether a body reads at all can depend on the grouping.
        if let Some(now) = live.regrouped(source) {
            return Ok(Ingested::Regrouped(now));
        }
        let applied = match open_locked(&mut tx, org, live.id, UNPARSED_KEY).await? {
            Some(open) => {
                bump(&mut tx, org, open.id).await?;
                Applied::on(Status::Unparsed, open.id)
            }
            None if !live.enabled => Applied::just(Status::Paused),
            None if live.held => Applied::just(Status::Held),
            None => {
                let title = format!("{} sent an alert we could not read", live.name);
                let detail = clean_block(reason, MAX_DETAIL_CHARS);
                let id = open_incident(
                    &mut tx,
                    org,
                    &live,
                    UNPARSED_KEY,
                    &title,
                    detail.as_deref(),
                    IncidentSeverity::Major,
                    None,
                )
                .await?;
                Applied::on(Status::Unparsed, id).signalling(id, NotificationReason::Opened)
            }
        };
        stamp_event(&mut tx, org, live.id).await?;
        tx.commit().await.map_err(db("ingest: commit"))?;
        Ok(Ingested::Applied(applied))
    }
}

async fn lock_source(tx: &mut Tx<'_>, org: OrgId, source: Uuid) -> Result<Option<Live>> {
    advisory_xact_lock(&mut **tx, &alert_source_lock_key(source))
        .await
        .map_err(db("ingest: source lock"))?;
    // An org deleted since its token was checked takes nothing new.
    sqlx::query_as(
        "SELECT s.id, s.name, s.enabled, s.plan_hold_at IS NOT NULL AS held, s.grouping \
         FROM alert_sources s \
         JOIN organizations o ON o.id = s.org_id AND o.deleted_at IS NULL \
         WHERE s.id = $1 AND s.org_id = $2",
    )
    .bind(source)
    .bind(org.0)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db("ingest: read source"))
}

async fn open_for(tx: &mut Tx<'_>, org: OrgId, source: Uuid, key: &str) -> Result<Option<Open>> {
    sqlx::query_as(
        "SELECT i.id, i.severity, a.fired_at, a.reported_severity \
         FROM incidents i LEFT JOIN incident_alert_state a ON a.incident_id = i.id \
         WHERE i.org_id = $1 AND i.alert_source_id = $2 AND i.dedup_key = $3 \
           AND i.origin = 'alert' AND i.ended_at IS NULL",
    )
    .bind(org.0)
    .bind(source)
    .bind(key)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db("ingest: open incident"))
}

/// The key's open incident, held under its own lock and read again under it:
/// a person's resolve takes only that lock, so one that landed first leaves
/// nothing open here and the firing opens a fresh incident.
async fn open_locked(tx: &mut Tx<'_>, org: OrgId, source: Uuid, key: &str) -> Result<Option<Open>> {
    let Some(seen) = open_for(tx, org, source, key).await? else {
        return Ok(None);
    };
    advisory_xact_lock(&mut **tx, &incident_lock_key(seen.id))
        .await
        .map_err(db("ingest: incident lock"))?;
    open_for(tx, org, source, key).await
}

/// Open incidents counted against the cap: the platform's own two are not.
async fn open_count(tx: &mut Tx<'_>, org: OrgId, source: Uuid) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*) FROM incidents \
         WHERE org_id = $1 AND alert_source_id = $2 AND origin = 'alert' \
           AND ended_at IS NULL AND dedup_key NOT IN ($3, $4)",
    )
    .bind(org.0)
    .bind(source)
    .bind(OVERFLOW_KEY)
    .bind(UNPARSED_KEY)
    .fetch_one(&mut **tx)
    .await
    .map_err(db("ingest: count open"))
}

async fn insert_incident(
    tx: &mut Tx<'_>,
    org: OrgId,
    source: Uuid,
    key: &str,
    title: &str,
    detail: Option<&str>,
    severity: IncidentSeverity,
) -> Result<Uuid> {
    // Internal, counting no downtime, owed its first page in the same commit
    // so a dropped signal is caught by the owed-page sweep.
    sqlx::query_scalar(
        "INSERT INTO incidents \
             (org_id, started_at, status_at_start, origin, state, severity, urgency, title, \
              error_sample, visibility, paging_enabled, counts_as_downtime, page_owed_at, \
              alert_source_id, dedup_key) \
         VALUES ($1, now(), 'down', 'alert', 'triggered', $2, $3, $4, $5, 'internal', true, \
                 false, now(), $6, $7) \
         RETURNING id",
    )
    .bind(org.0)
    .bind(severity.as_db_str())
    .bind(urgency(severity).as_db_str())
    .bind(title)
    .bind(detail)
    .bind(source)
    .bind(key)
    .fetch_one(&mut **tx)
    .await
    .map_err(db("ingest: open"))
}

async fn insert_state(
    tx: &mut Tx<'_>,
    org: OrgId,
    incident: Uuid,
    event: Option<&AlertEvent>,
) -> Result<()> {
    let (firing, fired_at, labels, annotations, links, severity) = match event {
        Some(e) => (
            e.firing.max(1),
            e.at,
            serde_json::json!(e.labels),
            serde_json::json!(e.annotations),
            serde_json::json!(e.links),
            e.severity,
        ),
        None => (
            1,
            None,
            serde_json::json!({}),
            serde_json::json!({}),
            serde_json::json!([]),
            IncidentSeverity::Major,
        ),
    };
    sqlx::query(
        "INSERT INTO incident_alert_state \
             (incident_id, org_id, last_event_at, firing_count, fired_at, labels, annotations, \
              links, reported_severity) \
         VALUES ($1, $2, now(), $3, $4, $5, $6, $7, $8)",
    )
    .bind(incident)
    .bind(org.0)
    .bind(firing as i32)
    .bind(fired_at)
    .bind(labels)
    .bind(annotations)
    .bind(links)
    .bind(severity.as_db_str())
    .execute(&mut **tx)
    .await
    .map_err(db("ingest: alert state"))?;
    Ok(())
}

async fn opened_event(tx: &mut Tx<'_>, org: OrgId, incident: Uuid, live: &Live) -> Result<()> {
    let message = format!("Opened by an alert from {}", live.name);
    insert_event_tx(
        tx,
        org,
        incident,
        IncidentEventKind::Triggered,
        Actor::System,
        Some(&message),
    )
    .await
}

/// Opens an incident for `key` with its state row and its first timeline
/// line. `event` is what the tool sent; the platform's own incidents about a
/// source, the summary at the cap and the unparsed one, have none.
#[allow(clippy::too_many_arguments)]
async fn open_incident(
    tx: &mut Tx<'_>,
    org: OrgId,
    live: &Live,
    key: &str,
    title: &str,
    detail: Option<&str>,
    severity: IncidentSeverity,
    event: Option<&AlertEvent>,
) -> Result<Uuid> {
    let id = insert_incident(tx, org, live.id, key, title, detail, severity).await?;
    insert_state(tx, org, id, event).await?;
    opened_event(tx, org, id, live).await?;
    Ok(id)
}

/// A firing for a key with nothing open: opens its incident, or at the cap
/// counts it on the summary.
async fn fire(tx: &mut Tx<'_>, org: OrgId, live: &Live, event: &AlertEvent) -> Result<Applied> {
    if open_count(tx, org, live.id).await? >= MAX_OPEN_PER_SOURCE {
        return overflow(tx, org, live).await;
    }
    let id = open_incident(
        tx,
        org,
        live,
        &event.key,
        &event.title,
        event.detail.as_deref(),
        event.severity,
        Some(event),
    )
    .await?;
    Ok(Applied::on(Status::Opened, id).signalling(id, NotificationReason::Opened))
}

/// A firing for a key already open never pages: reminders are ours to send.
/// What changed is written at once; a repeat that changes nothing writes at
/// most once a minute, so `repeat_count` counts the repeats recorded. A worse
/// severity is raised and said on the timeline.
async fn repeat(
    tx: &mut Tx<'_>,
    org: OrgId,
    live: &Live,
    open: Open,
    event: &AlertEvent,
) -> Result<Applied> {
    // Raised only when the tool says worse than it ever has and worse than
    // the incident stands, so a person who lowered it is not overruled by the
    // tool repeating itself, and one who raised it is never lowered. A
    // person's edit takes no lock, so the write lands only on the severity
    // read here.
    let reported = open
        .reported_severity
        .as_deref()
        .map(IncidentSeverity::from_db_str)
        .unwrap_or_default();
    let current = IncidentSeverity::from_db_str(&open.severity);
    let raised = event.severity > reported
        && event.severity > current
        && sqlx::query(
            "UPDATE incidents SET severity = $3, urgency = $4, updated_at = now() \
             WHERE id = $1 AND org_id = $2 AND severity = $5",
        )
        .bind(open.id)
        .bind(org.0)
        .bind(event.severity.as_db_str())
        .bind(urgency(event.severity).as_db_str())
        .bind(&open.severity)
        .execute(&mut **tx)
        .await
        .map_err(db("ingest: raise severity"))?
        .rows_affected()
            == 1;
    if raised {
        let message = format!(
            "Severity raised to {} by an alert from {}",
            event.severity.as_db_str(),
            live.name
        );
        insert_event_tx(
            tx,
            org,
            open.id,
            IncidentEventKind::SeverityChanged,
            Actor::System,
            Some(&message),
        )
        .await?;
    }
    sqlx::query(
        "UPDATE incident_alert_state SET \
             last_event_at = now(), repeat_count = repeat_count + 1, firing_count = $3, \
             fired_at = GREATEST(fired_at, $4), labels = $5, annotations = $6, links = $7, \
             reported_severity = $9 \
         WHERE incident_id = $1 AND org_id = $2 \
           AND (firing_count <> $3 OR fired_at IS DISTINCT FROM GREATEST(fired_at, $4) \
                OR labels IS DISTINCT FROM $5 OR annotations IS DISTINCT FROM $6 \
                OR links IS DISTINCT FROM $7 OR reported_severity <> $9 \
                OR last_event_at < now() - make_interval(secs => $8))",
    )
    .bind(open.id)
    .bind(org.0)
    .bind(event.firing.max(1) as i32)
    .bind(event.at)
    .bind(serde_json::json!(event.labels))
    .bind(serde_json::json!(event.annotations))
    .bind(serde_json::json!(event.links))
    .bind(COALESCE_SECS)
    .bind(event.severity.max(reported).as_db_str())
    .execute(&mut **tx)
    .await
    .map_err(db("ingest: repeat"))?;
    Ok(Applied::on(Status::Repeat, open.id))
}

/// Counts a refused key on the source's summary incident, opening it the
/// first time. It pages once; every refusal after is counted on it.
async fn overflow(tx: &mut Tx<'_>, org: OrgId, live: &Live) -> Result<Applied> {
    if let Some(open) = open_locked(tx, org, live.id, OVERFLOW_KEY).await? {
        bump(tx, org, open.id).await?;
        return Ok(Applied::on(Status::Capped, open.id));
    }
    let title = format!(
        "{} hit its limit of {MAX_OPEN_PER_SOURCE} open alerts",
        live.name
    );
    let detail = "New alerts from this source open nothing until some of its incidents \
                  close. Each one refused is counted here, and the tool sends it again at \
                  its next repeat.";
    let id = open_incident(
        tx,
        org,
        live,
        OVERFLOW_KEY,
        &title,
        Some(detail),
        IncidentSeverity::Major,
        None,
    )
    .await?;
    Ok(Applied::on(Status::Capped, id).signalling(id, NotificationReason::Opened))
}

/// Closes the summary once the source is under its cap again.
async fn close_summary_if_room(
    tx: &mut Tx<'_>,
    org: OrgId,
    live: &Live,
    applied: Applied,
) -> Result<Applied> {
    // One index probe for the common case, a source that never overflowed.
    if open_for(tx, org, live.id, OVERFLOW_KEY).await?.is_none()
        || open_count(tx, org, live.id).await? >= MAX_OPEN_PER_SOURCE
    {
        return Ok(applied);
    }
    let Some(summary) = open_locked(tx, org, live.id, OVERFLOW_KEY).await? else {
        return Ok(applied);
    };
    let note = format!("{} is back under its limit of open alerts", live.name);
    match resolve_in_tx(tx, org, summary.id, Actor::System, note).await? {
        LifecycleOutcome::Updated(_) => {
            Ok(applied.signalling(summary.id, NotificationReason::Resolved))
        }
        _ => Ok(applied),
    }
}

/// Counts one more event on an incident's state row.
async fn bump(tx: &mut Tx<'_>, org: OrgId, incident: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE incident_alert_state SET last_event_at = now(), repeat_count = repeat_count + 1 \
         WHERE incident_id = $1 AND org_id = $2",
    )
    .bind(incident)
    .bind(org.0)
    .execute(&mut **tx)
    .await
    .map_err(db("ingest: count"))?;
    Ok(())
}

/// Whether a resolve dated `at` is a late copy rather than the one that ends
/// the incident: dated more than [`RESOLVE_SKEW_SECS`] before the latest
/// firing seen, or dated the same as a resolve already applied to the key, or
/// more than that skew before one. This incident's own resolves before a
/// person reopened it count. Alerts in one group can carry times from
/// servers on different clocks, so a resolve within the skew of an applied
/// one that is not its copy is fresh. Each comparison is the sender's clock
/// against itself, so a firing that reached us late still closes on its
/// resolve.
async fn stale(
    tx: &mut Tx<'_>,
    org: OrgId,
    source: Uuid,
    key: &str,
    open: &Open,
    at: DateTime<Utc>,
) -> Result<bool> {
    let skew = chrono::Duration::seconds(RESOLVE_SKEW_SECS);
    if open.fired_at.is_some_and(|fired| at < fired - skew) {
        return Ok(true);
    }
    sqlx::query_scalar(
        "SELECT EXISTS ( \
             SELECT 1 FROM incidents i \
             JOIN incident_alert_state a ON a.incident_id = i.id \
             WHERE i.org_id = $1 AND i.alert_source_ref = $2 AND i.dedup_key = $3 \
               AND i.origin = 'alert' \
               AND ($4 = ANY(a.resolved_ats) OR $5 < ANY(a.resolved_ats)))",
    )
    .bind(org.0)
    .bind(source)
    .bind(key)
    .bind(at)
    .bind(at + skew)
    .fetch_one(&mut **tx)
    .await
    .map_err(db("ingest: earlier resolve"))
}

/// Closes the key's incident unless the resolve is stale. A resolve with no
/// date closes in arrival order.
async fn resolve(
    tx: &mut Tx<'_>,
    org: OrgId,
    live: &Live,
    key: &str,
    open: Open,
    at: Option<DateTime<Utc>>,
) -> Result<Applied> {
    if let Some(at) = at
        && stale(tx, org, live.id, key, &open, at).await?
    {
        return Ok(Applied {
            incident: Some(open.id),
            ..Applied::ignored("older than the incident it would close")
        });
    }
    let note = format!("Resolved by an alert from {}", live.name);
    if !matches!(
        resolve_in_tx(tx, org, open.id, Actor::System, note).await?,
        LifecycleOutcome::Updated(_)
    ) {
        return Ok(Applied {
            incident: Some(open.id),
            ..Applied::ignored("the incident had already closed")
        });
    }
    if let Some(at) = at {
        sqlx::query(
            "UPDATE incident_alert_state SET resolved_ats = array_append(resolved_ats, $3) \
             WHERE incident_id = $1 AND org_id = $2",
        )
        .bind(open.id)
        .bind(org.0)
        .bind(at)
        .execute(&mut **tx)
        .await
        .map_err(db("ingest: resolve time"))?;
    }
    Ok(Applied::on(Status::Resolved, open.id).signalling(open.id, NotificationReason::Resolved))
}

async fn stamp_event(tx: &mut Tx<'_>, org: OrgId, source: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE alert_sources SET last_event_at = now() \
         WHERE id = $1 AND org_id = $2 \
           AND (last_event_at IS NULL OR last_event_at < now() - make_interval(secs => $3))",
    )
    .bind(source)
    .bind(org.0)
    .bind(COALESCE_SECS)
    .execute(&mut **tx)
    .await
    .map_err(db("ingest: stamp source"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sender's key in this namespace is stored hashed, which is all that
    /// keeps it off the platform's own incidents.
    #[test]
    fn the_platforms_own_keys_sit_under_the_reserved_prefix() {
        for key in [OVERFLOW_KEY, UNPARSED_KEY] {
            assert!(key.starts_with(crate::intake::RESERVED_KEY_PREFIX));
            assert_ne!(crate::intake::stored_key(key).as_deref(), Some(key));
        }
    }
}
