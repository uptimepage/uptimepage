//! Why a monitor is failing right now, read the same way by every surface
//! that says so.

use chrono::{DateTime, Duration, Utc};

use crate::domain::{CheckAdvice, CheckStatus, OrgId, Target, explain_check_error};
use crate::storage::traits::{ClampedRange, RegionNewest};
use crate::storage::{ResultsStore, TimeRange};

use super::status::FOLD_LOOKBACK_HOURS;

/// How far back "has it passed since it was set up" looks. A monitor failing
/// for longer than this reads as never having passed.
const SETUP_LOOKBACK_DAYS: i64 = 7;
/// Distinct errors past this many fold into a count.
const CAUSES_SHOWN: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureCause {
    pub text: String,
    pub regions: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureNote {
    /// Distinct errors, the one most regions report first.
    pub causes: Vec<FailureCause>,
    pub more_causes: usize,
    /// Regions the causes were read from. One needs no count beside its cause.
    pub region_count: usize,
    /// Set while every region fails the same way. A hint also needs no region
    /// to have passed since the monitor was created or last edited: after a
    /// pass the check has shown it fits the target, and the same error is news
    /// about the target instead. A diagnostic's guidance rests on evidence, so
    /// it holds either way.
    pub advice: Option<CheckAdvice>,
}

impl FailureNote {
    /// `region` narrows the causes to that region's own. The advice is a
    /// statement about the monitor, so it reads every region either way.
    fn fold(rows: &[RegionNewest], region: Option<&str>, now: DateTime<Utc>) -> Option<Self> {
        // The denominator the status fold votes by: a region that stopped
        // reporting must not freeze its last verdict into the answer.
        let reporting_since = now - Duration::hours(FOLD_LOOKBACK_HOURS);
        let reporting: Vec<&RegionNewest> =
            rows.iter().filter(|r| r.at >= reporting_since).collect();
        let passed = rows.iter().any(|r| r.passed);
        let advice =
            CheckAdvice::agreed(reporting.iter().map(|r| {
                CheckAdvice::of(r.error.as_deref(), r.diagnostic.as_ref().map(|d| d.kind))
            }))
            .filter(|advice| advice.hint().is_none() || !passed);

        let in_view: Vec<&RegionNewest> = reporting
            .into_iter()
            .filter(|r| region.is_none_or(|want| r.region == want))
            .collect();
        let mut causes: Vec<FailureCause> = Vec::new();
        for row in in_view.iter().filter(|r| r.status != CheckStatus::Up) {
            let Some(text) = explain_check_error(row.error.as_deref(), row.diagnostic.as_ref())
            else {
                continue;
            };
            match causes.iter_mut().find(|c| c.text == text) {
                Some(cause) => cause.regions += 1,
                None => causes.push(FailureCause { text, regions: 1 }),
            }
        }
        if causes.is_empty() {
            return None;
        }
        causes.sort_by_key(|c| std::cmp::Reverse(c.regions));
        let more_causes = causes.len().saturating_sub(CAUSES_SHOWN);
        causes.truncate(CAUSES_SHOWN);
        Some(Self {
            causes,
            more_causes,
            region_count: in_view.len(),
            advice,
        })
    }
}

/// `status` is the one the caller shows for the monitor; nothing is read for
/// one that is not failing. Best-effort: a failed read costs the note, never
/// the page around it.
pub async fn failure_note(
    results: &dyn ResultsStore,
    org: OrgId,
    target: &Target,
    status: &str,
    region: Option<&str>,
) -> Option<FailureNote> {
    if !matches!(status, "down" | "error" | "degraded") {
        return None;
    }
    let now = Utc::now();
    let since = target
        .created_at
        .max(target.updated_at)
        .max(now - Duration::days(SETUP_LOOKBACK_DAYS));
    // Current state, not history browsing, so the plan's window does not
    // apply. A probe clock a little ahead of ours must not hide its newest run.
    let range = ClampedRange::unclamped(TimeRange {
        from: since,
        to: now + Duration::minutes(5),
    });
    let rows = results
        .newest_by_region(org, target.id, range)
        .await
        .map_err(|err| tracing::warn!(error = %err, "failure note unavailable"))
        .ok()?;
    FailureNote::fold(&rows, region, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CheckDiagnostic, CheckDiagnosticKind, CheckHint, CheckResult, DiagnosticConfidence,
        EdgeProvider,
    };
    use crate::storage::{InMemorySink, ResultSink};
    use uuid::Uuid;

    const SILENT_HOST: CheckAdvice = CheckAdvice::Hint(CheckHint::IcmpNoReply);

    fn region(name: &str, status: CheckStatus, error: Option<&str>) -> RegionNewest {
        RegionNewest {
            region: name.into(),
            at: Utc::now(),
            status,
            error: error.map(str::to_owned),
            diagnostic: None,
            passed: false,
        }
    }

    fn silent(name: &str) -> RegionNewest {
        region(
            name,
            CheckStatus::Down,
            Some("no echo reply from 192.0.2.1 within 3000ms"),
        )
    }

    fn fold(rows: &[RegionNewest]) -> Option<FailureNote> {
        FailureNote::fold(rows, None, Utc::now())
    }

    #[test]
    fn regions_failing_alike_share_one_cause_and_a_hint() {
        let note = fold(&[silent("eu"), silent("us")]).unwrap();
        assert_eq!(
            note.causes,
            vec![FailureCause {
                text: "no echo reply from 192.0.2.1 within 3000ms".into(),
                regions: 2
            }]
        );
        assert_eq!(note.region_count, 2);
        assert_eq!(note.advice, Some(SILENT_HOST));
    }

    #[test]
    fn a_region_that_passes_makes_it_a_path_problem() {
        let mut up = region("us", CheckStatus::Up, None);
        up.passed = true;
        let note = fold(&[silent("eu"), up]).unwrap();
        assert_eq!(note.causes.len(), 1);
        assert_eq!(note.region_count, 2);
        assert_eq!(note.advice, None);
    }

    #[test]
    fn regions_failing_differently_list_each_cause_without_a_hint() {
        let refused = region("ap", CheckStatus::Down, Some("connection refused"));
        let note = fold(&[refused, silent("eu"), silent("us")]).unwrap();
        assert_eq!(note.causes[0].regions, 2);
        assert_eq!(note.causes[1].text, "connection refused");
        assert_eq!(note.advice, None);
    }

    #[test]
    fn a_pass_since_setup_retires_the_hint_but_not_the_cause() {
        let mut once_up = silent("eu");
        once_up.passed = true;
        let note = fold(&[once_up, silent("us")]).unwrap();
        assert_eq!(note.causes[0].regions, 2);
        assert_eq!(note.advice, None);
    }

    #[test]
    fn a_region_that_stopped_reporting_does_not_vote() {
        let mut stale = region("ap", CheckStatus::Down, Some("connection refused"));
        stale.at = Utc::now() - Duration::hours(FOLD_LOOKBACK_HOURS + 1);
        let note = fold(&[stale.clone(), silent("eu")]).unwrap();
        assert_eq!(note.region_count, 1);
        assert_eq!(note.advice, Some(SILENT_HOST));

        stale.passed = true;
        let note = fold(&[stale, silent("eu")]).unwrap();
        assert_eq!(
            note.advice, None,
            "a pass anywhere since setup still counts"
        );
    }

    fn blocked(name: &str) -> RegionNewest {
        RegionNewest {
            diagnostic: Some(CheckDiagnostic::access_interference(
                DiagnosticConfidence::High,
                Some(EdgeProvider::Cloudflare),
                Vec::new(),
            )),
            ..region(name, CheckStatus::Down, Some("unexpected status 403"))
        }
    }

    /// The better-evidenced failure must not end up with less help than a
    /// plain 403, which gets the generic hint.
    #[test]
    fn a_diagnosed_block_shows_its_guidance_in_place_of_the_hint() {
        let note = fold(&[blocked("eu"), blocked("us")]).unwrap();
        assert_eq!(
            note.causes[0].text,
            "unexpected status 403 · access-policy block detected at the Cloudflare edge"
        );
        let advice = note.advice.unwrap();
        assert_eq!(
            advice,
            CheckAdvice::Diagnosed(CheckDiagnosticKind::AccessInterference)
        );
        assert_ne!(advice.text(), CheckHint::AccessRefused.text());

        let plain = region("eu", CheckStatus::Down, Some("unexpected status 403"));
        assert_eq!(
            fold(std::slice::from_ref(&plain)).unwrap().advice,
            Some(CheckAdvice::Hint(CheckHint::AccessRefused))
        );
        // One region saw the block page, the other only the status.
        assert_eq!(fold(&[blocked("us"), plain]).unwrap().advice, None);
    }

    /// Evidence holds whatever the monitor's history: this is not setup advice.
    #[test]
    fn a_diagnosed_block_keeps_its_guidance_on_a_monitor_that_has_passed() {
        let mut once_up = blocked("eu");
        once_up.passed = true;
        let note = fold(&[once_up]).unwrap();
        assert_eq!(
            note.advice,
            Some(CheckAdvice::Diagnosed(
                CheckDiagnosticKind::AccessInterference
            ))
        );
    }

    #[test]
    fn a_region_filter_narrows_causes_and_leaves_the_advice_alone() {
        let rows = [silent("eu"), silent("us")];
        let note = FailureNote::fold(&rows, Some("eu"), Utc::now()).unwrap();
        assert_eq!(note.region_count, 1);
        assert_eq!(note.causes[0].regions, 1);
        assert_eq!(note.advice, Some(SILENT_HOST));
    }

    fn result(target: &Target, ago: Duration, error: Option<&str>) -> CheckResult {
        let mut result = CheckResult::error(target.id, Uuid::nil(), error.unwrap_or_default());
        result.timestamp = Utc::now() - ago;
        match error {
            Some(_) => result.status = CheckStatus::Down,
            None => {
                result.status = CheckStatus::Up;
                result.error = None;
            }
        }
        result
    }

    const SILENCE: Option<&str> = Some("no echo reply from 192.0.2.1 within 3000ms");

    async fn note_for(
        target: &Target,
        status: &str,
        results: &[CheckResult],
    ) -> Option<FailureNote> {
        let sink = InMemorySink::new();
        sink.write_batch(results).await.unwrap();
        failure_note(&sink, OrgId(Uuid::nil()), target, status, None).await
    }

    fn monitor(created: Duration, edited: Duration) -> Target {
        let mut target = Target::sample(std::time::Duration::from_secs(60));
        target.created_at = Utc::now() - created;
        target.updated_at = Utc::now() - edited;
        target
    }

    #[tokio::test]
    async fn only_a_failing_status_is_explained() {
        let target = monitor(Duration::hours(1), Duration::hours(1));
        let failing = [result(&target, Duration::minutes(1), SILENCE)];
        for status in ["up", "", "waiting", "late"] {
            assert_eq!(note_for(&target, status, &failing).await, None, "{status}");
        }
        for status in ["down", "error", "degraded"] {
            let note = note_for(&target, status, &failing).await;
            assert_eq!(note.unwrap().advice, Some(SILENT_HOST), "{status}");
        }
    }

    #[tokio::test]
    async fn a_pass_counts_only_since_the_last_edit() {
        let target = monitor(Duration::days(30), Duration::hours(1));
        let before_edit = [
            result(&target, Duration::hours(2), None),
            result(&target, Duration::minutes(30), SILENCE),
        ];
        let note = note_for(&target, "down", &before_edit).await.unwrap();
        assert_eq!(note.advice, Some(SILENT_HOST));

        let after_edit = [
            result(&target, Duration::minutes(50), None),
            result(&target, Duration::minutes(10), SILENCE),
        ];
        let note = note_for(&target, "down", &after_edit).await.unwrap();
        assert_eq!(note.advice, None);
        assert_eq!(note.causes.len(), 1);
    }

    #[tokio::test]
    async fn a_pass_older_than_the_lookback_no_longer_counts() {
        let target = monitor(Duration::days(30), Duration::days(30));
        let recent = Duration::days(SETUP_LOOKBACK_DAYS - 1);
        let old = Duration::days(SETUP_LOOKBACK_DAYS + 1);
        for (passed, advice) in [(recent, None), (old, Some(SILENT_HOST))] {
            let results = [
                result(&target, passed, None),
                result(&target, Duration::minutes(1), SILENCE),
            ];
            let note = note_for(&target, "down", &results).await.unwrap();
            assert_eq!(note.advice, advice, "pass {passed} ago");
        }
    }

    #[test]
    fn nothing_failing_is_no_note() {
        assert_eq!(fold(&[region("eu", CheckStatus::Up, None)]), None);
        assert_eq!(fold(&[]), None);
    }

    #[test]
    fn causes_past_the_cap_fold_into_a_count() {
        let rows: Vec<RegionNewest> = ["a", "b", "c", "d"]
            .iter()
            .map(|n| {
                region(
                    n,
                    CheckStatus::Down,
                    Some(&format!("unexpected status 50{n}")),
                )
            })
            .collect();
        let note = fold(&rows).unwrap();
        assert_eq!(note.causes.len(), CAUSES_SHOWN);
        assert_eq!(note.more_causes, 1);
    }
}
