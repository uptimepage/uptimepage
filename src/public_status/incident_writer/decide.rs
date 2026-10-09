//! The pure decision: what the recent results say should open or close, with
//! no database and no clock of its own.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use uuid::Uuid;

use crate::domain::{CheckResult, CheckStatus, Recovered};

use super::{NewOpenIncident, OpenIncident};

/// Decision produced by [`decide`].
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    None,
    Open(NewOpenIncident),
    Close {
        incident_id: Uuid,
        ended_at: DateTime<Utc>,
    },
    /// Regions that confirmed the failure since the incident opened.
    Widen {
        incident_id: Uuid,
        regions: Vec<String>,
    },
    /// A confirmed failure went hard down on an incident that opened softer;
    /// the cause moves with it.
    Escalate {
        incident_id: Uuid,
        error_sample: Option<String>,
    },
    /// The outage is over and its recovery is being held. While `since` is
    /// set paging pauses and the incident stops counting as downtime from
    /// then; `None` once the recovery is lost.
    Recovering {
        incident_id: Uuid,
        since: Option<DateTime<Utc>>,
    },
    /// A stretch it was back up in until a failure inside the recovery period
    /// took it down again. Kept on the incident so it never counts as
    /// downtime; the recovering mark it began, if any, is dropped.
    Relapsed {
        incident_id: Uuid,
        recovered: Recovered,
    },
}

/// How long a recovery must hold before its incident closes, and the clock it
/// is judged by.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hold {
    pub period: ChronoDuration,
    /// The monitor's check interval.
    pub interval: ChronoDuration,
    pub now: DateTime<Utc>,
}

impl Hold {
    /// Closes on the confirming checks alone.
    pub const NONE: Self = Self {
        period: ChronoDuration::zero(),
        interval: ChronoDuration::zero(),
        now: DateTime::<Utc>::MIN_UTC,
    };

    /// Held once the period has passed since the recovery began and every
    /// check due inside it has passed too. A check due after the period is not
    /// waited for, so a monitor that checks less often than its hold closes
    /// when the hold is up rather than a whole interval later.
    fn has_held(&self, began: DateTime<Utc>, last_seen: DateTime<Utc>) -> bool {
        self.period <= ChronoDuration::zero()
            || (self.now - began >= self.period && last_seen - began >= self.period - self.interval)
    }
}

struct Verdict<'a> {
    region: &'a str,
    bad: &'a [CheckResult],
}

/// Single-region convenience over [`decide_multi`]: one region, combined
/// any-down policy. Kept for call sites and tests that reason about a flat
/// result stream. Returns at most one [`Action`].
///
/// **Idempotency**: referentially transparent. Any `Action::Open` it returns
/// assumes the caller has just verified there is no open incident — re-running
/// after the write falls through to `Action::None`.
pub fn decide(open: Option<&OpenIncident>, results: &[CheckResult], flap_threshold: u32) -> Action {
    let Some(target_id) = results.first().map(|r| r.target_id) else {
        return Action::None;
    };
    let opens: Vec<OpenIncident> = open.cloned().into_iter().collect();
    let by_region = [(String::new(), results.to_vec())];
    decide_multi(target_id, &opens, &by_region, flap_threshold, 1, Hold::NONE)
        .into_iter()
        .next()
        .unwrap_or(Action::None)
}

/// Pure region-aware decision. Each `(region, results)` group is one region's
/// checks ascending by time; `opens` is every open incident for the target.
/// `confirmations` is the per-region consecutive-bad run needed; `quorum` is how
/// many regions must agree before the combined incident opens (clamped to the
/// live region count so it can never be unreachable). `hold` is how long the
/// recovery must hold before the incident closes. Returns the writes to apply;
/// an empty vec means nothing to do.
pub fn decide_multi(
    target_id: Uuid,
    opens: &[OpenIncident],
    by_region: &[(String, Vec<CheckResult>)],
    confirmations: u32,
    quorum: usize,
    hold: Hold,
) -> Vec<Action> {
    let threshold = (confirmations as usize).max(1);

    let verdicts: Vec<Verdict> = by_region
        .iter()
        .map(|(region, results)| Verdict {
            region,
            bad: trailing_bad_run(results),
        })
        .collect();

    let quorum = quorum.clamp(1, verdicts.len().max(1));
    let mut bad: Vec<&Verdict> = verdicts
        .iter()
        .filter(|v| v.bad.len() >= threshold)
        .collect();
    bad.sort_by_key(|v| v.bad[0].timestamp);
    let combined = opens.iter().find(|i| i.region.is_none());

    match combined {
        None => {
            if bad.len() >= quorum {
                // region = None: one whole-target incident, so its key must be
                // region-independent or the next tick re-opens it.
                let trigger = bad[quorum - 1];
                let origin = bad[0];
                // Worst across every confirmed bad run — a degraded origin
                // region must not mask a concurrently hard-down region.
                let status_at_start = worst_status(&bad).unwrap_or(CheckStatus::Down);
                let (regions_down, regions_up) = split_regions(&bad, &verdicts);
                vec![Action::Open(NewOpenIncident {
                    target_id,
                    started_at: trigger.bad[0].timestamp,
                    status_at_start,
                    check_count: origin.bad.len() as u32,
                    error_sample: incident_error_sample(&bad, quorum),
                    region: None,
                    regions_down,
                    regions_up,
                })]
            } else {
                vec![]
            }
        }
        Some(inc) => {
            let tally = Tally::of(inc, by_region, threshold);
            let mut actions = Vec::new();
            // Every stretch in view it was back up in before the failure
            // returned stays out of the downtime: kept once, however many
            // scans see it and however many came and went between two scans.
            let mut marked = inc.recovering_since;
            for recovered in tally.back_up_stretches(quorum) {
                if recovered.from < inc.started_at {
                    continue;
                }
                if marked == Some(recovered.from) {
                    marked = None;
                } else if inc.recovered.iter().any(|r| r.from == recovered.from) {
                    continue;
                }
                actions.push(Action::Relapsed {
                    incident_id: inc.id,
                    recovered,
                });
            }
            // Down again with the recovery it marked out of view. Silence
            // settles nothing, so it leaves the mark alone.
            if marked.is_some() && tally.down_now >= quorum {
                actions.push(Action::Recovering {
                    incident_id: inc.id,
                    since: None,
                });
                marked = None;
            }
            // Close once the outage is over and its recovery has held;
            // ended_at is when it ended, so the hold is not counted as downtime.
            // Until then the incident is recovering from that moment.
            if let Some(r) = tally.recovery(quorum)
                && r.ended > inc.started_at
            {
                if !r.unsettled && hold.has_held(r.ended, r.last_seen) {
                    actions.push(Action::Close {
                        incident_id: inc.id,
                        ended_at: r.ended,
                    });
                } else if marked != Some(r.ended) {
                    actions.push(Action::Recovering {
                        incident_id: inc.id,
                        since: Some(r.ended),
                    });
                }
                return actions;
            }
            // The breakdown only grows: a region one check behind the quorum
            // joins once it confirms, silence adds nothing, and a recovery is
            // the close's business, so the row keeps the worst the outage was.
            // The status follows the same rule. Below quorum the outage is
            // over and only waiting out its recovery, so a lone region failing
            // then is not part of it.
            if bad.len() >= quorum {
                let (confirmed, _) = split_regions(&bad, &verdicts);
                let regions: Vec<String> = confirmed
                    .into_iter()
                    .filter(|r| !inc.regions_down.contains(r))
                    .collect();
                if !regions.is_empty() {
                    actions.push(Action::Widen {
                        incident_id: inc.id,
                        regions,
                    });
                }
            }
            // Only to down: an error is as often our probe as the service, so
            // it never turns a degraded incident into an outage. Judged as an
            // opening is: the quorum confirms a failure, the worst sets its status.
            if bad.len() >= quorum
                && inc.worst_status != CheckStatus::Down
                && worst_status(&bad) == Some(CheckStatus::Down)
            {
                actions.push(Action::Escalate {
                    incident_id: inc.id,
                    error_sample: incident_error_sample(&bad, quorum),
                });
            }
            actions
        }
    }
}

/// A region's confirmed state: down after `threshold` failures in a row, up
/// after `threshold` passes in a row, dated from the first of those passes. A
/// shorter run changes neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Confirmed {
    Unknown,
    Up(DateTime<Utc>),
    Down,
}

/// A stretch a region was confirmed down; open-ended while it still is.
type DownSpan = (DateTime<Utc>, Option<DateTime<Utc>>);

/// The stretches one region was confirmed down, each dated from the first
/// check of the run that confirmed it to the first check of the run that
/// confirmed the recovery (`None` while still down), and where it stands now.
/// `down_at_start` is for a region the open incident already counts as down,
/// whose failing run may have begun before the window.
fn confirmed_down_spans(
    results: &[CheckResult],
    threshold: usize,
    down_at_start: bool,
) -> (Vec<DownSpan>, Confirmed) {
    let mut state = if down_at_start {
        Confirmed::Down
    } else {
        Confirmed::Unknown
    };
    let mut since = down_at_start.then_some(DateTime::<Utc>::MIN_UTC);
    let mut spans = Vec::new();
    let mut run: Option<(bool, DateTime<Utc>, usize)> = None;
    for r in results {
        let bad = r.status.is_bad();
        let (start, len) = match run {
            Some((was_bad, start, len)) if was_bad == bad => (start, len + 1),
            _ => (r.timestamp, 1),
        };
        run = Some((bad, start, len));
        if len != threshold {
            continue;
        }
        match (bad, state) {
            (true, Confirmed::Up(_) | Confirmed::Unknown) => {
                state = Confirmed::Down;
                since = Some(start);
            }
            (false, Confirmed::Down) => {
                state = Confirmed::Up(start);
                if let Some(from) = since.take() {
                    spans.push((from, Some(start)));
                }
            }
            (false, Confirmed::Unknown) => state = Confirmed::Up(start),
            _ => {}
        }
    }
    if let Some(from) = since {
        spans.push((from, None));
    }
    (spans, state)
}

/// What [`Tally::recovery`] found.
struct Recovery {
    ended: DateTime<Utc>,
    /// The newest result in view, which the hold is judged against.
    last_seen: DateTime<Utc>,
    /// Failures not yet confirmed could still bring the regions down back to
    /// the quorum, so the recovery cannot be called held until they settle.
    unsettled: bool,
}

/// Each region's confirmed state over the window, counted up for the open
/// incident: the regions it was counted in start out down.
struct Tally {
    /// Confirmed-down stretches as count changes, sorted so a region failing
    /// at the instant another recovers is counted first and the handover is
    /// not mistaken for the outage ending.
    events: Vec<(DateTime<Utc>, i32)>,
    down_now: usize,
    failing_unconfirmed: usize,
    undecided: bool,
    witnessed: Option<DateTime<Utc>>,
    all_up_since: Option<DateTime<Utc>>,
    last_seen: Option<DateTime<Utc>>,
}

impl Tally {
    fn of(inc: &OpenIncident, by_region: &[(String, Vec<CheckResult>)], threshold: usize) -> Self {
        let mut t = Self {
            events: Vec::new(),
            down_now: 0,
            failing_unconfirmed: 0,
            undecided: false,
            witnessed: None,
            all_up_since: None,
            last_seen: None,
        };
        for (region, results) in by_region {
            let Some(last) = results.last() else {
                continue;
            };
            t.last_seen = t.last_seen.max(Some(last.timestamp));
            // Without a breakdown (one unnamed region, or a row from before the
            // breakdown was kept) every region is taken as part of the outage.
            let in_outage = inc.regions_down.is_empty() || inc.regions_down.contains(region);
            let (spans, state) = confirmed_down_spans(results, threshold, in_outage);
            match state {
                Confirmed::Down => t.down_now += 1,
                Confirmed::Up(since) => {
                    t.all_up_since = t.all_up_since.max(Some(since));
                    if in_outage {
                        t.witnessed = t.witnessed.max(Some(since));
                    }
                }
                Confirmed::Unknown => t.undecided = true,
            }
            if state != Confirmed::Down && !trailing_bad_run(results).is_empty() {
                t.failing_unconfirmed += 1;
            }
            for (from, to) in spans {
                t.events.push((from, 1));
                if let Some(to) = to {
                    t.events.push((to, -1));
                }
            }
        }
        t.events.sort_by_key(|(at, delta)| (*at, -delta));
        t
    }

    /// When the outage ended: the last moment the regions confirmed down fell
    /// below the quorum. Runs shorter than a confirmation change nothing, so a
    /// stray failed check neither restarts the hold nor moves the end, and a
    /// lone region failing below the quorum is not the outage. An outage that
    /// confirms again inside the hold keeps the incident open, and its
    /// recovery restarts the wait. When the quorum is never reached in view,
    /// the end is the latest recovery of a region the outage was counted in.
    ///
    /// The recovery has to be confirmed by a region the outage was counted in.
    /// A region that never failed says nothing about one that stopped
    /// reporting, so silence cannot carry the count below the quorum while a
    /// region is still failing. Only when every region the outage was counted
    /// in has gone quiet do the regions still reporting decide, and then every
    /// one of them has to be confirmed up: the end is when the last of them was.
    fn recovery(&self, quorum: usize) -> Option<Recovery> {
        if self.down_now >= quorum {
            return None;
        }
        let onset = match self.witnessed {
            Some(recovered) => {
                let quorum = quorum as i32;
                let mut down = 0;
                let mut ended = None;
                for &(at, delta) in &self.events {
                    if down >= quorum && down + delta < quorum {
                        ended = Some(at);
                    }
                    down += delta;
                }
                ended.unwrap_or(recovered)
            }
            None if self.down_now == 0 && !self.undecided => self.all_up_since?,
            None => return None,
        };
        Some(Recovery {
            ended: onset,
            last_seen: self.last_seen?,
            unsettled: self.down_now + self.failing_unconfirmed >= quorum,
        })
    }

    /// Each stretch the regions confirmed down were below the quorum before
    /// they reached it again: a recovery a failure ended.
    fn back_up_stretches(&self, quorum: usize) -> Vec<Recovered> {
        let quorum = quorum as i32;
        let mut down = 0;
        let mut up_since = None;
        let mut out = Vec::new();
        for &(at, delta) in &self.events {
            let was_down = down >= quorum;
            down += delta;
            match (was_down, down >= quorum) {
                (true, false) => up_since = Some(at),
                (false, true) => {
                    if let Some(from) = up_since.take()
                        && at > from
                    {
                        out.push(Recovered { from, until: at });
                    }
                }
                _ => {}
            }
        }
        out
    }
}

fn worst_status(bad: &[&Verdict]) -> Option<CheckStatus> {
    bad.iter()
        .flat_map(|v| v.bad.iter().map(|r| r.status))
        .max_by_key(|s| s.severity_rank())
}

/// Confirmed regions in the order they failed, then the reporting regions that
/// have not confirmed. Region names are empty on a single-region stream.
fn split_regions(bad: &[&Verdict], verdicts: &[Verdict]) -> (Vec<String>, Vec<String>) {
    let down: Vec<String> = bad
        .iter()
        .map(|v| v.region)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .collect();
    let up = verdicts
        .iter()
        .map(|v| v.region)
        .filter(|r| !r.is_empty() && !down.iter().any(|d| d == r))
        .map(str::to_string)
        .collect();
    (down, up)
}

/// Cause as stated to notifications and incident views. The per-result error
/// is left untouched for API consumers.
fn incident_error_sample(bad: &[&Verdict<'_>], quorum: usize) -> Option<String> {
    // Newest failure per region only: an earlier page must not outlive a
    // change in how the edge is failing.
    let diagnosed: Vec<_> = bad
        .iter()
        .filter_map(|verdict| {
            let result = verdict.bad.last()?;
            result
                .diagnostic
                .as_ref()
                .map(|diagnostic| (result, diagnostic))
        })
        .collect();
    let best = diagnosed.iter().copied().max_by_key(|(_, candidate)| {
        diagnosed
            .iter()
            .filter(|(_, other)| {
                other.kind == candidate.kind
                    && other.confidence == candidate.confidence
                    && other.provider == candidate.provider
            })
            .count()
    });

    if let Some((sample, diagnostic)) = best {
        let matching_regions = diagnosed
            .iter()
            .filter(|(_, other)| {
                other.kind == diagnostic.kind
                    && other.confidence == diagnostic.confidence
                    && other.provider == diagnostic.provider
            })
            .count();
        // Same bar the incident itself cleared, so one vendor page cannot
        // label a multi-region outage.
        if matching_regions >= quorum {
            let mut parts = Vec::with_capacity(4);
            if let Some(error) = sample.error.as_deref() {
                parts.push(error.to_owned());
            }
            parts.push(diagnostic.summary());
            // Ahead of the tally: notifications clip this sample, and the fix
            // is worth more to the reader than the vote count.
            parts.push(diagnostic.guidance().to_string());
            if bad.len() > 1 {
                parts.push(format!(
                    "{matching_regions}/{} failing regions agree",
                    bad.len()
                ));
            }
            return Some(parts.join(" · "));
        }
    }

    // Nothing cleared the bar: report the protocol failure rather than guess,
    // newest first for the same reason as above.
    bad.iter().find_map(|verdict| {
        verdict
            .bad
            .iter()
            .rev()
            .find_map(|result| result.error.clone())
    })
}

fn trailing_bad_run(results: &[CheckResult]) -> &[CheckResult] {
    let split = results
        .iter()
        .rposition(|r| !r.status.is_bad())
        .map(|i| i + 1)
        .unwrap_or(0);
    &results[split..]
}
