//! Which monitor incidents on one status page its subscribers hear about as
//! one outage, and when they hear. No database and no clock: the planner loads
//! a page's open outages and new incidents, runs these rules, and writes what
//! they changed.
//!
//! Every decision reads the incidents' own recorded times, never when the
//! planner happened to run, so a pass that runs late reaches the same answer
//! one that ran on time would have.

use chrono::{DateTime, Duration, Utc};
use uuid::Uuid;

/// Incidents on different components read as one fault only when they
/// overlap and began within this long of each other, so one long incident
/// does not swallow every unrelated one that happens during it. The page
/// history folds by the same rule.
pub const STARTED_TOGETHER: Duration = Duration::minutes(15);

/// How long the started mail waits after an outage opens, so monitors that
/// fail in the same minute are in one mail.
pub const GATHER: Duration = Duration::seconds(60);

/// How long a page stays quiet after its last member closed before the
/// outage ends. The same monitor failing again inside it rejoins, so a flap
/// sends one pair of mails, not one per flap.
pub const SETTLE: Duration = Duration::minutes(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub incident_id: Uuid,
    pub target_id: Uuid,
    pub started_at: DateTime<Utc>,
    /// When the monitor recovered; `None` while it is down.
    pub ended_at: Option<DateTime<Utc>>,
    /// When the membership closed; `None` while it is open.
    pub closed_at: Option<DateTime<Utc>>,
}

impl Member {
    fn end(&self) -> DateTime<Utc> {
        self.ended_at.unwrap_or(DateTime::<Utc>::MAX_UTC)
    }

    /// The history fold's rule for incidents on different components.
    fn together(&self, other: &Member) -> bool {
        self.started_at < other.end()
            && other.started_at < self.end()
            && (self.started_at - other.started_at).abs() < STARTED_TOGETHER
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outage {
    pub opened_at: DateTime<Utc>,
    pub started_at: DateTime<Utc>,
    pub recovered_at: Option<DateTime<Utc>>,
    pub members: Vec<Member>,
}

impl Outage {
    fn opened_by(candidate: &Candidate) -> Self {
        let mut outage = Self {
            opened_at: candidate.said_at,
            started_at: candidate.member.started_at,
            recovered_at: None,
            members: vec![candidate.member.clone()],
        };
        outage.refresh();
        outage
    }

    /// Kept current after every change to the members, so the next decision
    /// reads the state the last one left.
    pub fn refresh(&mut self) {
        if let Some(first) = self.members.iter().map(|m| m.started_at).min() {
            self.started_at = first;
        }
        self.recovered_at = self
            .members
            .iter()
            .map(|m| m.closed_at)
            .collect::<Option<Vec<_>>>()
            .and_then(|closes| closes.into_iter().max());
    }

    fn takes(&self, candidate: &Member) -> bool {
        self.members.iter().any(|m| m.together(candidate))
            || self.recovered_at.is_some_and(|recovered| {
                candidate.started_at <= recovered + SETTLE
                    && self
                        .members
                        .iter()
                        .any(|m| m.target_id == candidate.target_id)
            })
    }

    pub fn any_open(&self) -> bool {
        self.members.iter().any(|m| m.closed_at.is_none())
    }

    pub fn resolvable(&self, now: DateTime<Utc>) -> bool {
        self.recovered_at.is_some_and(|r| r + SETTLE <= now)
    }
}

/// An incident on the page that no outage holds yet, open or already closed.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub member: Member,
    /// When the page first said something about it.
    pub said_at: DateTime<Utc>,
}

/// Places every candidate, oldest first, in the newest outage that takes it,
/// or in a new one. Returns, per candidate, its incident and the index in
/// `outages` it went to; new outages are appended.
pub fn join(outages: &mut Vec<Outage>, mut candidates: Vec<Candidate>) -> Vec<(Uuid, usize)> {
    candidates.sort_by_key(|c| (c.member.started_at, c.member.incident_id));
    candidates
        .into_iter()
        .map(|candidate| {
            let taker = outages
                .iter()
                .enumerate()
                .filter(|(_, o)| o.takes(&candidate.member))
                .max_by_key(|(_, o)| o.opened_at)
                .map(|(i, _)| i);
            let incident = candidate.member.incident_id;
            let at = match taker {
                Some(i) => {
                    outages[i].members.push(candidate.member);
                    outages[i].refresh();
                    i
                }
                None => {
                    outages.push(Outage::opened_by(&candidate));
                    outages.len() - 1
                }
            };
            (incident, at)
        })
        .collect()
}

/// What the page now says about one member's incident.
#[derive(Debug, Clone, Copy)]
pub struct MemberFacts {
    pub ended_at: Option<DateTime<Utc>>,
    /// When the close was posted; after the recovery hold.
    pub resolved_at: Option<DateTime<Utc>>,
    /// Still a component of the page, not held, page served.
    pub shows: bool,
}

/// Brings a member up to date: closed when its incident ended or it left the
/// page, open again when its incident was reopened and the page shows it.
/// Returns whether anything changed.
/// A member that left the page while down keeps no end: whatever its
/// incident does later is not the page's news.
pub fn reconcile(member: &mut Member, facts: MemberFacts, now: DateTime<Utc>) -> bool {
    let before = member.clone();
    match (facts.ended_at, member.closed_at) {
        (Some(ended), None) => {
            member.ended_at = Some(ended);
            member.closed_at = Some(facts.resolved_at.unwrap_or(ended));
        }
        (None, None) if !facts.shows => member.closed_at = Some(now),
        (None, Some(_)) if facts.shows => {
            member.ended_at = None;
            member.closed_at = None;
        }
        _ => {}
    }
    *member != before
}

impl Outage {
    /// Still in its first minute: nothing is told about it, and it does not
    /// end yet.
    pub fn gathering(&self, now: DateTime<Utc>) -> bool {
        now < self.opened_at + GATHER
    }

    /// Its started mail is due, if none went out yet: past the gather,
    /// something still down, and recent enough to be news. A blip that closed
    /// inside the gather has no started mail, but a failure that rejoins it
    /// later does.
    pub fn starts(&self, now: DateTime<Utc>, lookback: Duration) -> bool {
        !self.gathering(now) && self.any_open() && self.opened_at >= now - lookback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hms: &str) -> DateTime<Utc> {
        format!("2026-10-21T{hms}Z").parse().unwrap()
    }

    fn incident(n: u128, target: u128, start: &str, end: Option<&str>) -> Member {
        Member {
            incident_id: Uuid::from_u128(n),
            target_id: Uuid::from_u128(1000 + target),
            started_at: at(start),
            ended_at: end.map(at),
            closed_at: None,
        }
    }

    /// Open, or closed when its close was posted at `closed`.
    fn candidate(member: Member, said: &str, closed: Option<&str>) -> Candidate {
        Candidate {
            member: Member {
                closed_at: closed.map(at),
                ..member
            },
            said_at: at(said),
        }
    }

    fn placed(outages: &[Outage]) -> Vec<Vec<u128>> {
        outages
            .iter()
            .map(|o| o.members.iter().map(|m| m.incident_id.as_u128()).collect())
            .collect()
    }

    #[test]
    fn five_monitors_down_together_are_one_outage() {
        let mut outages = Vec::new();
        let starts = ["19:02:26", "19:02:28", "19:02:30", "19:02:30", "19:02:37"];
        let candidates = starts
            .iter()
            .enumerate()
            .map(|(i, s)| candidate(incident(i as u128, i as u128, s, None), "19:03:51", None))
            .collect();
        join(&mut outages, candidates);
        assert_eq!(placed(&outages), [vec![0, 1, 2, 3, 4]]);
        assert_eq!(outages[0].started_at, at("19:02:26"));
        assert_eq!(outages[0].opened_at, at("19:03:51"));
        assert_eq!(outages[0].recovered_at, None);
    }

    #[test]
    fn a_cascade_ten_minutes_apart_joins_and_a_long_down_member_swallows_nothing_later() {
        let mut outages = Vec::new();
        join(
            &mut outages,
            vec![
                candidate(incident(1, 1, "10:00:00", None), "10:01:00", None),
                candidate(incident(2, 2, "10:10:00", None), "10:11:00", None),
            ],
        );
        assert_eq!(placed(&outages), [vec![1, 2]], "overlap and 10 minutes");

        // Down since the morning; a new failure at noon is news of its own.
        let mut outages = Vec::new();
        join(
            &mut outages,
            vec![candidate(
                incident(1, 1, "08:00:00", None),
                "08:01:00",
                None,
            )],
        );
        join(
            &mut outages,
            vec![candidate(
                incident(2, 2, "12:00:00", None),
                "12:01:00",
                None,
            )],
        );
        assert_eq!(placed(&outages), [vec![1], vec![2]]);
    }

    #[test]
    fn a_flap_inside_the_settle_rejoins_and_another_monitor_does_not() {
        let mut outages = Vec::new();
        join(
            &mut outages,
            vec![candidate(
                incident(1, 1, "10:00:00", Some("10:01:00")),
                "10:00:30",
                Some("10:02:00"),
            )],
        );
        assert_eq!(outages[0].recovered_at, Some(at("10:02:00")));
        join(
            &mut outages,
            vec![
                candidate(incident(2, 1, "10:05:00", None), "10:05:30", None),
                candidate(incident(3, 2, "10:05:10", None), "10:05:40", None),
            ],
        );
        assert_eq!(
            placed(&outages),
            [vec![1, 2, 3]],
            "the flap reopens it, and the other monitor overlaps the flap"
        );

        // Another monitor alone inside the settle overlaps nothing.
        let mut outages = Vec::new();
        join(
            &mut outages,
            vec![candidate(
                incident(1, 1, "10:00:00", Some("10:01:00")),
                "10:00:30",
                Some("10:02:00"),
            )],
        );
        join(
            &mut outages,
            vec![candidate(
                incident(2, 2, "10:04:00", None),
                "10:04:30",
                None,
            )],
        );
        assert_eq!(placed(&outages), [vec![1], vec![2]]);
    }

    #[test]
    fn the_settle_bound_holds_on_a_pass_that_runs_late() {
        let first = || {
            candidate(
                incident(1, 1, "10:00:00", Some("10:01:00")),
                "10:00:30",
                Some("10:02:00"),
            )
        };
        let mut inside = vec![];
        join(&mut inside, vec![first()]);
        join(
            &mut inside,
            vec![candidate(
                incident(2, 1, "10:06:59", None),
                "10:07:30",
                None,
            )],
        );
        assert_eq!(
            placed(&inside),
            [vec![1, 2]],
            "10:06:59 is inside 10:02 + 5"
        );

        let mut outside = vec![];
        join(&mut outside, vec![first()]);
        join(
            &mut outside,
            vec![candidate(
                incident(2, 1, "10:07:01", None),
                "10:07:30",
                None,
            )],
        );
        assert_eq!(placed(&outside), [vec![1], vec![2]]);
        assert!(outside[0].resolvable(at("10:20:00")));
    }

    #[test]
    fn two_finished_flaps_found_in_one_pass_are_one_outage() {
        let mut outages = Vec::new();
        join(
            &mut outages,
            vec![
                candidate(
                    incident(2, 1, "10:03:00", Some("10:04:00")),
                    "10:03:20",
                    Some("10:04:30"),
                ),
                candidate(
                    incident(1, 1, "10:00:00", Some("10:01:00")),
                    "10:00:20",
                    Some("10:01:30"),
                ),
            ],
        );
        assert_eq!(placed(&outages), [vec![1, 2]]);
        assert_eq!(outages[0].recovered_at, Some(at("10:04:30")));
        assert!(
            !outages[0].starts(at("10:30:00"), Duration::hours(24)),
            "both were over before anyone was told: one summary"
        );
    }

    #[test]
    fn a_closed_candidate_joins_by_its_times() {
        let mut outages = Vec::new();
        join(
            &mut outages,
            vec![candidate(
                incident(1, 1, "10:00:00", None),
                "10:01:00",
                None,
            )],
        );
        join(
            &mut outages,
            vec![candidate(
                incident(2, 2, "10:02:00", Some("10:02:40")),
                "10:02:30",
                Some("10:03:00"),
            )],
        );
        assert_eq!(placed(&outages), [vec![1, 2]]);
        assert_eq!(outages[0].recovered_at, None, "the first is still open");
    }

    #[test]
    fn a_blip_inside_the_gather_starts_nothing_until_it_fails_again() {
        let mut outages = Vec::new();
        join(
            &mut outages,
            vec![candidate(
                incident(1, 1, "09:00:10", Some("09:00:40")),
                "09:00:30",
                Some("09:00:55"),
            )],
        );
        let lookback = Duration::hours(24);
        assert!(outages[0].gathering(at("09:01:00")));
        assert!(!outages[0].starts(at("09:01:30"), lookback));
        assert!(!outages[0].resolvable(at("09:05:54")));
        assert!(outages[0].resolvable(at("09:05:55")));

        // The same monitor fails again inside the settle and stays down.
        join(
            &mut outages,
            vec![candidate(
                incident(2, 1, "09:03:00", None),
                "09:03:30",
                None,
            )],
        );
        assert_eq!(placed(&outages), [vec![1, 2]]);
        assert!(outages[0].starts(at("09:04:00"), lookback));
    }

    #[test]
    fn an_open_outage_starts_once_gathered_and_only_inside_the_lookback() {
        let mut outages = Vec::new();
        join(
            &mut outages,
            vec![candidate(
                incident(1, 1, "10:00:00", None),
                "10:01:00",
                None,
            )],
        );
        let o = &outages[0];
        assert!(!o.starts(at("10:01:59"), Duration::hours(24)));
        assert!(o.starts(at("10:02:00"), Duration::hours(24)));
        assert!(!o.starts(at("12:00:00"), Duration::hours(1)));
    }

    #[test]
    fn members_close_leave_and_reopen() {
        let now = at("11:00:00");
        let mut m = incident(1, 1, "10:00:00", None);
        let down = MemberFacts {
            ended_at: None,
            resolved_at: None,
            shows: true,
        };
        assert!(!reconcile(&mut m, down, now));

        let ended = MemberFacts {
            ended_at: Some(at("10:30:00")),
            resolved_at: Some(at("10:33:00")),
            shows: true,
        };
        assert!(reconcile(&mut m, ended, now));
        assert_eq!(m.closed_at, Some(at("10:33:00")));
        assert!(!reconcile(&mut m, ended, now), "idempotent");

        assert!(reconcile(&mut m, down, now), "reopened");
        assert_eq!(m.closed_at, None);

        let gone = MemberFacts {
            ended_at: None,
            resolved_at: None,
            shows: false,
        };
        assert!(reconcile(&mut m, gone, now));
        assert_eq!(m.closed_at, Some(now), "removed while down closes now");
        assert!(
            !reconcile(&mut m, ended, now),
            "its later end is not the page's news"
        );
        assert_eq!(m.ended_at, None);
    }

    #[test]
    fn a_closing_member_keeps_recovery_current() {
        let mut outages = Vec::new();
        join(
            &mut outages,
            vec![
                candidate(incident(1, 1, "10:00:00", None), "10:01:00", None),
                candidate(incident(2, 2, "10:00:05", None), "10:01:00", None),
            ],
        );
        let o = &mut outages[0];
        let facts = |end: &str, posted: &str| MemberFacts {
            ended_at: Some(at(end)),
            resolved_at: Some(at(posted)),
            shows: true,
        };
        reconcile(
            &mut o.members[0],
            facts("10:05:00", "10:07:00"),
            at("10:08:00"),
        );
        o.refresh();
        assert_eq!(o.recovered_at, None);
        reconcile(
            &mut o.members[1],
            facts("10:06:00", "10:08:00"),
            at("10:09:00"),
        );
        o.refresh();
        assert_eq!(o.recovered_at, Some(at("10:08:00")));
    }
}
