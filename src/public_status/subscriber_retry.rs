//! What happens to a subscriber delivery after each answer. An attempt is a
//! try that reached the provider or the receiver and failed for a reason
//! that can change; pacing, quotas and our own account's trouble say nothing
//! about the subscriber, so they wait without using one up. Attempts space
//! the tries out. Only a webhook runs out of them, since its receiver is the
//! subscriber's own; mail that keeps failing on the provider's side waits
//! until the notice expires.

use std::time::Duration;

use chrono::{DateTime, Utc};
use hyper::StatusCode;

use crate::email::EmailError;
use crate::http_outbound::PostAnswer;

/// Attempts a webhook delivery gets before it is given up.
pub const WEBHOOK_MAX_ATTEMPTS: i32 = 8;

const BACKOFF_BASE: Duration = Duration::from_secs(30);
const BACKOFF_CAP: Duration = Duration::from_secs(30 * 60);
const GATE_RETRY: Duration = Duration::from_secs(30);
const QUOTA_RETRY: Duration = Duration::from_secs(60 * 60);
const REFUSED_RETRY: Duration = Duration::from_secs(15 * 60);
const IN_PROGRESS_RETRY: Duration = Duration::from_secs(10);
/// A receiver asking for less is still given this long, so a `Retry-After:
/// 0` cannot spin. One that always asks to wait is ended by the expiry.
const RECEIVER_WAIT_MIN: Duration = BACKOFF_BASE;
const RECEIVER_WAIT_CAP: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    Sent,
    /// This message will not be accepted as it is.
    GiveUp(String),
    Retry {
        error: String,
        counts: bool,
        after: Duration,
    },
}

impl Next {
    fn wait(error: String, after: Duration) -> Self {
        Self::Retry {
            error,
            counts: false,
            after,
        }
    }

    fn backoff(error: String, attempts: i32) -> Self {
        Self::Retry {
            error,
            counts: true,
            after: backoff(attempts + 1),
        }
    }
}

/// The provider may have taken a request that got this answer, so it is
/// only ever sent again as it was.
pub fn outcome_unknown(err: &EmailError) -> bool {
    matches!(err, EmailError::Transport(_) | EmailError::InProgress(_))
}

/// One mail's own result. `attempts` is the number already used before it.
pub fn after_mail(err: &EmailError, attempts: i32, now: DateTime<Utc>) -> Next {
    let error = err.to_string();
    match err {
        EmailError::RateLimited { retry_after, .. } | EmailError::Deferred { retry_after } => {
            Next::wait(error, *retry_after)
        }
        EmailError::GateUnavailable => Next::wait(error, GATE_RETRY),
        EmailError::QuotaExceeded { until, .. } => Next::wait(
            error,
            until
                .map(|t| (t - now).to_std().unwrap_or(Duration::ZERO))
                .unwrap_or(QUOTA_RETRY),
        ),
        EmailError::Refused(_) => Next::wait(error, REFUSED_RETRY),
        EmailError::Rejected(_) => Next::GiveUp(error),
        EmailError::InProgress(_) => Next::wait(error, IN_PROGRESS_RETRY),
        EmailError::Transport(_) => Next::backoff(error, attempts),
    }
}

/// An answer to a whole batch. The provider checks each mail on its own, so
/// a batch refused as a whole is refused for our request or our account,
/// not for its subscribers, and waits like a refused key.
pub fn after_batch(err: &EmailError, attempts: i32, now: DateTime<Utc>) -> Next {
    match err {
        EmailError::Rejected(_) => Next::wait(err.to_string(), REFUSED_RETRY),
        _ => after_mail(err, attempts, now),
    }
}

/// `answer` is the receiver's reply, or why there was none.
pub fn after_webhook(answer: &Result<PostAnswer, String>, attempts: i32) -> Next {
    let answer = match answer {
        Ok(answer) if answer.status.is_success() => return Next::Sent,
        Ok(answer) => answer,
        Err(error) => return Next::backoff(error.clone(), attempts),
    };
    let error = format!("endpoint returned {}: {}", answer.status, answer.body);
    if answer.status == StatusCode::GONE {
        return Next::GiveUp(error);
    }
    let asks_to_wait = matches!(
        answer.status,
        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
    );
    match answer.retry_after {
        Some(wait) if asks_to_wait => {
            Next::wait(error, wait.clamp(RECEIVER_WAIT_MIN, RECEIVER_WAIT_CAP))
        }
        _ => Next::backoff(error, attempts),
    }
}

/// 30 s doubling per attempt, capped at 30 minutes, plus up to half again at
/// random so a burst that failed together does not come back together.
fn backoff(attempt: i32) -> Duration {
    let base = backoff_floor(attempt);
    base + base.mul_f64(fastrand::f64() / 2.0)
}

fn backoff_floor(attempt: i32) -> Duration {
    let doublings = (attempt.max(1) - 1).min(16) as u32;
    BACKOFF_BASE.saturating_mul(1 << doublings).min(BACKOFF_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-10-21T19:03:51Z".parse().unwrap()
    }

    fn waits(next: Next) -> (bool, Duration) {
        match next {
            Next::Retry { counts, after, .. } => (counts, after),
            other => panic!("expected a retry, got {other:?}"),
        }
    }

    #[test]
    fn pacing_quota_and_our_account_wait_without_an_attempt() {
        let rate = EmailError::RateLimited {
            retry_after: Duration::from_secs(7),
            detail: "429".into(),
        };
        assert_eq!(
            waits(after_mail(&rate, 2, now())),
            (false, Duration::from_secs(7))
        );
        let deferred = EmailError::Deferred {
            retry_after: Duration::from_secs(40),
        };
        assert_eq!(
            waits(after_mail(&deferred, 2, now())),
            (false, Duration::from_secs(40))
        );
        assert_eq!(
            waits(after_mail(&EmailError::GateUnavailable, 2, now())),
            (false, GATE_RETRY)
        );
        let daily = EmailError::QuotaExceeded {
            until: Some("2026-10-22T00:00:00Z".parse().unwrap()),
            detail: "daily".into(),
        };
        assert_eq!(
            waits(after_mail(&daily, 2, now())),
            (false, Duration::from_secs(4 * 3600 + 56 * 60 + 9)),
            "next UTC midnight"
        );
        let monthly = EmailError::QuotaExceeded {
            until: None,
            detail: "monthly".into(),
        };
        assert_eq!(waits(after_mail(&monthly, 2, now())), (false, QUOTA_RETRY));
        let reset = EmailError::QuotaExceeded {
            until: Some("2026-10-21T19:00:00Z".parse().unwrap()),
            detail: "daily".into(),
        };
        assert_eq!(
            waits(after_mail(&reset, 2, now())),
            (false, Duration::ZERO),
            "already reset"
        );
        assert_eq!(
            waits(after_mail(&EmailError::Refused("403".into()), 2, now())),
            (false, REFUSED_RETRY)
        );
    }

    #[test]
    fn a_rejected_message_is_given_up_and_an_unknown_outcome_is_sent_again_as_is() {
        assert!(matches!(
            after_mail(&EmailError::Rejected("422".into()), 0, now()),
            Next::GiveUp(_)
        ));
        assert_eq!(
            waits(after_batch(&EmailError::Rejected("422".into()), 0, now())),
            (false, REFUSED_RETRY),
            "a whole batch refused is our request, not its subscribers"
        );
        let in_progress = EmailError::InProgress("409".into());
        assert_eq!(
            waits(after_mail(&in_progress, 0, now())),
            (false, IN_PROGRESS_RETRY)
        );
        let timeout = EmailError::Transport("timeout".into());
        assert!(waits(after_mail(&timeout, 0, now())).0);
        assert!(outcome_unknown(&in_progress) && outcome_unknown(&timeout));
        assert!(!outcome_unknown(&EmailError::Rejected("422".into())));
    }

    #[test]
    fn a_receiver_asking_to_wait_is_waited_for_and_anything_else_counts() {
        let answer = |status: u16, retry_after: Option<u64>| {
            Ok(PostAnswer {
                status: StatusCode::from_u16(status).unwrap(),
                retry_after: retry_after.map(Duration::from_secs),
                body: String::new(),
            })
        };
        assert_eq!(after_webhook(&answer(204, None), 0), Next::Sent);
        assert_eq!(
            waits(after_webhook(&answer(429, Some(90)), 0)),
            (false, Duration::from_secs(90))
        );
        assert_eq!(
            waits(after_webhook(&answer(503, Some(99_999)), 0)),
            (false, RECEIVER_WAIT_CAP)
        );
        assert_eq!(
            waits(after_webhook(&answer(429, Some(0)), 0)),
            (false, RECEIVER_WAIT_MIN)
        );
        assert!(
            waits(after_webhook(&answer(429, None), 0)).0,
            "no wait named"
        );
        assert!(waits(after_webhook(&answer(404, Some(5)), 0)).0);
        assert!(matches!(
            after_webhook(&answer(410, None), 0),
            Next::GiveUp(_)
        ));
        assert!(waits(after_webhook(&answer(500, None), 0)).0);
        assert!(waits(after_webhook(&Err("connection refused".into()), 0)).0);
    }

    #[test]
    fn backoff_doubles_to_its_cap_with_up_to_half_again_of_jitter() {
        assert_eq!(backoff_floor(1), Duration::from_secs(30));
        assert_eq!(backoff_floor(2), Duration::from_secs(60));
        assert_eq!(backoff_floor(4), Duration::from_secs(240));
        assert_eq!(backoff_floor(7), BACKOFF_CAP);
        assert_eq!(backoff_floor(50), BACKOFF_CAP);
        let spread: Vec<Duration> = (0..200).map(|_| backoff(3)).collect();
        assert!(
            spread
                .iter()
                .all(|d| (Duration::from_secs(120)..=Duration::from_secs(180)).contains(d))
        );
        assert!(spread.iter().any(|d| *d != spread[0]), "jittered");
    }
}
