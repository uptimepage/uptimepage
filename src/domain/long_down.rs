use std::time::Duration;

/// When a monitor failing from every region counts as long-down, and how often
/// it is checked while it stays that way.
pub struct LongDown {
    pub after: Duration,
    pub interval: Duration,
}

pub const LONG_DOWN: LongDown = LongDown {
    after: Duration::from_secs(3 * 86_400),
    interval: Duration::from_secs(15 * 60),
};
