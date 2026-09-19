//! What a job reports about its own run. `/ping/{token}` alone is a success;
//! the trailing segment carries the rest.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PingSignal {
    Start,
    Success,
    Fail,
}

impl PingSignal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Success => "success",
            Self::Fail => "fail",
        }
    }

    pub fn as_enum8(self) -> i8 {
        match self {
            Self::Start => 1,
            Self::Success => 2,
            Self::Fail => 3,
        }
    }

    /// Closes an open `start`.
    pub fn is_finish(self) -> bool {
        matches!(self, Self::Success | Self::Fail)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ping {
    pub signal: PingSignal,
    pub exit_code: Option<u8>,
}

impl Ping {
    pub const SUCCESS: Self = Self {
        signal: PingSignal::Success,
        exit_code: None,
    };

    /// Numeric form is `curl $URL/$?`. Unknown words are rejected so a typo
    /// cannot report success.
    pub fn parse(segment: &str) -> Option<Self> {
        match segment {
            "start" => Some(Self {
                signal: PingSignal::Start,
                exit_code: None,
            }),
            "fail" => Some(Self {
                signal: PingSignal::Fail,
                exit_code: None,
            }),
            "success" => Some(Self::SUCCESS),
            _ => segment.parse::<u8>().ok().map(|code| Self {
                signal: if code == 0 {
                    PingSignal::Success
                } else {
                    PingSignal::Fail
                },
                exit_code: Some(code),
            }),
        }
    }
}

/// Kept whether or not the signal changed the verdict.
#[derive(Debug, Clone)]
pub struct HeartbeatPingRecord {
    pub org_id: uuid::Uuid,
    pub target_id: uuid::Uuid,
    pub received_at: chrono::DateTime<chrono::Utc>,
    pub signal: PingSignal,
    pub exit_code: Option<u8>,
    /// `/start`→finish of the run this signal closed.
    pub duration_ms: Option<u32>,
    /// Already truncated to what is kept.
    pub body: String,
}

/// Below this a median is one odd run away from wrong.
const MIN_CADENCE_SAMPLES: u32 = 5;

const TOO_LOOSE_FACTOR: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedCadence {
    /// Gaps measured, not pings seen.
    pub samples: u32,
    pub median_gap: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CadenceAdvice {
    TooTight { suggested_period: Duration },
    TooLoose { suggested_period: Duration },
}

impl ObservedCadence {
    /// Judged against `down_after` (`period + grace`), not the period alone:
    /// grace exists to absorb jitter, so a job living inside it is not late.
    pub fn advice(&self, down_after: Duration) -> Option<CadenceAdvice> {
        if self.samples < MIN_CADENCE_SAMPLES || self.median_gap.is_zero() {
            return None;
        }
        if self.median_gap > down_after {
            return Some(CadenceAdvice::TooTight {
                suggested_period: round_up(self.median_gap),
            });
        }
        if down_after >= self.median_gap * TOO_LOOSE_FACTOR {
            return Some(CadenceAdvice::TooLoose {
                suggested_period: round_up(self.median_gap * 2),
            });
        }
        None
    }
}

/// Round to a number someone would type: 90 minutes, not 83.
fn round_up(d: Duration) -> Duration {
    // (ceiling, step)
    const STEPS: [(u64, u64); 4] = [(600, 60), (3600, 300), (21_600, 900), (u64::MAX, 3600)];
    let secs = d.as_secs();
    let step = STEPS
        .iter()
        .find(|(ceiling, _)| secs < *ceiling)
        .map_or(3600, |(_, step)| *step);
    Duration::from_secs(secs.div_ceil(step) * step)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Failure {
    pub at: DateTime<Utc>,
    pub exit_code: Option<u8>,
}

/// Latest-seen timestamps rather than state flags, so two nodes merge their
/// views by taking the newer of each without a shared lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PingState {
    /// Silence-rule anchor: the later of the last success and the re-arm point.
    pub success_at: DateTime<Utc>,
    pub start_at: Option<DateTime<Utc>>,
    pub fail: Option<Failure>,
}

impl PingState {
    pub fn from_success(success_at: DateTime<Utc>) -> Self {
        Self {
            success_at,
            start_at: None,
            fail: None,
        }
    }

    /// A newer success clears it, and so does a re-arm: both move `success_at`.
    pub fn failing(&self) -> Option<Failure> {
        self.fail.filter(|f| f.at > self.success_at)
    }

    /// `>=` against the anchor, because the wiring ping can be the start: it
    /// arms the monitor and opens the run in one statement, at one timestamp.
    pub fn run_open_since(&self) -> Option<DateTime<Utc>> {
        self.start_at
            .filter(|s| *s >= self.success_at && self.fail.is_none_or(|f| *s > f.at))
    }

    /// The snapshot may predate a ping this node already accepted.
    pub fn merge_newer(&mut self, other: Self) {
        if other.success_at > self.success_at {
            self.success_at = other.success_at;
        }
        if other.start_at > self.start_at {
            self.start_at = other.start_at;
        }
        if other.fail.map(|f| f.at) > self.fail.map(|f| f.at) {
            self.fail = other.fail;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_and_exit_codes_parse() {
        assert_eq!(Ping::parse("start").unwrap().signal, PingSignal::Start);
        assert_eq!(Ping::parse("fail").unwrap().signal, PingSignal::Fail);
        assert_eq!(Ping::parse("success").unwrap(), Ping::SUCCESS);
        assert_eq!(
            Ping::parse("0").unwrap(),
            Ping {
                signal: PingSignal::Success,
                exit_code: Some(0)
            }
        );
        assert_eq!(
            Ping::parse("137").unwrap(),
            Ping {
                signal: PingSignal::Fail,
                exit_code: Some(137)
            }
        );
    }

    #[test]
    fn unknown_segments_are_rejected() {
        for s in ["", "ok", "done", "-1", "256", "1 "] {
            assert!(Ping::parse(s).is_none(), "{s} must not parse");
        }
    }

    fn seen(samples: u32, median_s: u64) -> ObservedCadence {
        ObservedCadence {
            samples,
            median_gap: Duration::from_secs(median_s),
        }
    }

    #[test]
    fn a_job_slower_than_its_declaration_gets_a_workable_period() {
        let advice = seen(12, 4980).advice(Duration::from_secs(600));
        assert_eq!(
            advice,
            Some(CadenceAdvice::TooTight {
                suggested_period: Duration::from_secs(5400)
            }),
            "83 minutes observed should suggest 90, not 83"
        );
    }

    #[test]
    fn a_period_far_longer_than_the_job_names_the_blind_spot() {
        let advice = seen(40, 300).advice(Duration::from_secs(3600));
        assert_eq!(
            advice,
            Some(CadenceAdvice::TooLoose {
                suggested_period: Duration::from_secs(600)
            })
        );
    }

    #[test]
    fn a_declaration_that_fits_is_left_alone() {
        assert_eq!(seen(40, 290).advice(Duration::from_secs(300)), None);
        assert_eq!(seen(40, 300).advice(Duration::from_secs(900)), None);
    }

    /// period 300 + grace 1800: a 620s cadence never goes down, so telling its
    /// owner it pages them would be a lie.
    #[test]
    fn a_job_inside_its_grace_is_not_late() {
        assert_eq!(seen(40, 620).advice(Duration::from_secs(2100)), None);
    }

    #[test]
    fn thin_history_argues_nothing() {
        assert_eq!(
            seen(MIN_CADENCE_SAMPLES - 1, 4980).advice(Duration::from_secs(600)),
            None
        );
        // Pinged once: no gap at all.
        assert_eq!(seen(9, 0).advice(Duration::from_secs(600)), None);
    }

    #[test]
    fn suggestions_round_to_numbers_a_person_would_type() {
        let up = |s| round_up(Duration::from_secs(s)).as_secs();
        assert_eq!(up(90), 120, "under 10 min, to the minute");
        assert_eq!(up(1_000), 1_200, "under an hour, to 5 minutes");
        assert_eq!(up(4_980), 5_400, "under 6 hours, to 15 minutes");
        assert_eq!(up(90_000), 90_000, "past 6 hours, to the hour");
        assert_eq!(up(300), 300, "an exact step is already round");
    }
}
