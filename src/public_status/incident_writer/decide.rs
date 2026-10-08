//! The pure decision: what the recent results say should open or close, with
//! no database and no clock of its own.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use uuid::Uuid;

use crate::domain::{CheckResult, CheckStatus};

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
}

struct Verdict<'a> {
    region: &'a str,
    bad: &'a [CheckResult],
    good: &'a [CheckResult],
    /// The region failed somewhere in view, so its trailing good run is a
    /// recovery, not just a region that was never down.
    failed: bool,
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
    decide_multi(
        target_id,
        &opens,
        &by_region,
        flap_threshold,
        1,
        ChronoDuration::zero(),
    )
    .into_iter()
    .next()
    .unwrap_or(Action::None)
}

/// Pure region-aware decision. Each `(region, results)` group is one region's
/// checks ascending by time; `opens` is every open incident for the target.
/// `confirmations` is the per-region consecutive-bad run needed; `quorum` is how
/// many regions must agree before the combined incident opens (clamped to the
/// live region count so it can never be unreachable). `recovery` is how long
/// the recovery must hold before the incident closes. Returns the writes to
/// apply; an empty vec means nothing to do.
pub fn decide_multi(
    target_id: Uuid,
    opens: &[OpenIncident],
    by_region: &[(String, Vec<CheckResult>)],
    confirmations: u32,
    quorum: usize,
    recovery: ChronoDuration,
) -> Vec<Action> {
    let threshold = (confirmations as usize).max(1);

    let verdicts: Vec<Verdict> = by_region
        .iter()
        .map(|(region, results)| {
            let good = trailing_good_run(results);
            Verdict {
                region,
                bad: trailing_bad_run(results),
                good,
                failed: good.len() < results.len(),
            }
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
            // Close once below quorum and the recovery has held; ended_at is
            // when it began, so the hold is not counted as downtime.
            if bad.len() < quorum
                && let Some(ended) = recovered_at(&verdicts, threshold, recovery)
                && ended > inc.started_at
            {
                return vec![Action::Close {
                    incident_id: inc.id,
                    ended_at: ended,
                }];
            }
            // The breakdown only grows: a region one check behind the quorum
            // joins once it confirms, silence adds nothing, and a recovery is
            // the close's business, so the row keeps the worst the outage was.
            // The status follows the same rule. Below quorum the outage is
            // over and only waiting out its recovery, so a lone region failing
            // then is not part of it.
            let mut actions = Vec::new();
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

/// When the outage ended, once its recovery has held for `recovery`: the
/// latest onset among regions back up for a full confirmation run. A region up
/// throughout may never have failed, so it dates the end only alongside a
/// region seen recovering, or when every region has been up for the whole
/// window and the recovery itself is older than the window. A region failing
/// again restarts its own run, so a renewed outage restarts the hold.
fn recovered_at(
    verdicts: &[Verdict],
    threshold: usize,
    recovery: ChronoDuration,
) -> Option<DateTime<Utc>> {
    let held: Vec<&Verdict> = verdicts
        .iter()
        .filter(|v| v.good.len() >= threshold)
        .collect();
    let witnessed = held.iter().any(|v| v.failed);
    if !witnessed && verdicts.iter().any(|v| v.failed) {
        return None;
    }
    let onset = held.iter().map(|v| v.good[0].timestamp).max()?;
    let seen = held
        .iter()
        .filter_map(|v| v.good.last())
        .map(|r| r.timestamp)
        .max()?;
    (seen - onset >= recovery).then_some(onset)
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

fn trailing_good_run(results: &[CheckResult]) -> &[CheckResult] {
    let split = results
        .iter()
        .rposition(|r| r.status.is_bad())
        .map(|i| i + 1)
        .unwrap_or(0);
    &results[split..]
}
