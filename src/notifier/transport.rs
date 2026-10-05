use async_trait::async_trait;

use crate::error::Result;
use crate::notifier::event::IncidentNotice;

#[async_trait]
pub trait Notifier: Send + Sync {
    /// Page an incident lifecycle event (opened/resolved/reopened/escalated).
    async fn notify_incident(&self, notice: &IncidentNotice) -> Result<()>;

    /// Provider receipt captured by the preceding successful send, when the
    /// transport returns one to track for acknowledgement/cancel (Pushover
    /// emergency). `None` for every other transport. A notifier instance
    /// serves a single send, so the receipt belongs to that send.
    fn taken_receipt(&self) -> Option<String> {
        None
    }

    /// A notifier instance serves a single send, so the move belongs to it.
    fn taken_chat_migration(&self) -> Option<ChatMigration> {
        None
    }
}

/// String-scanned because transports flatten the vendor body into the error
/// text. Sign allowed, fraction dropped.
pub(crate) fn json_int_field(error: &str, key: &str) -> Option<i64> {
    let needle = format!("\"{key}\":");
    let rest = error[error.find(&needle)? + needle.len()..].trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMigration {
    pub from: String,
    pub to: String,
}
