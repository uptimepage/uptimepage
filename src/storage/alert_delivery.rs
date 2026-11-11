//! The delivery log's vocabulary: what one alert event did, and the line the
//! log keeps of it. A leaf, so the store traits and the ingest path both name
//! it without importing each other.

use chrono::{DateTime, Utc};
use uuid::Uuid;

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
    /// Not applied: the database failed or did not keep up. The sender is
    /// answered 503 and sends the body again.
    Unavailable,
    /// The source was deleted since its token was checked.
    Gone,
}

impl Status {
    /// What the tool sent did not take effect as it asked.
    pub const NOT_APPLIED: [Status; 5] = [
        Status::Paused,
        Status::Held,
        Status::Capped,
        Status::Unparsed,
        Status::Unavailable,
    ];

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
            Self::Unavailable => "unavailable",
            Self::Gone => "gone",
        }
    }
}

/// One line of a source's delivery log: one event of one request, or the
/// request itself when it carried none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub org_id: Uuid,
    pub source_id: Uuid,
    /// When the request arrived, shared by every event in it.
    pub received_at: DateTime<Utc>,
    pub status: Status,
    pub dedup_key: String,
    pub incident_id: Option<Uuid>,
    pub note: String,
    /// The start of the request's body, on its first line only, and empty
    /// when every event in it was a repeat.
    pub body: String,
}
