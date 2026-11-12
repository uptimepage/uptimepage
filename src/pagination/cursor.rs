//! Opaque keyset-pagination cursors for time-ordered list endpoints.
//!
//! A cursor is a base64-url-no-pad-encoded JSON blob carrying the sort keys
//! of the last returned row. Callers treat it as an opaque string — only the
//! server needs to know the schema. Switching from offset+limit to keyset
//! gives stable pagination under inserts (a new row arriving between pages
//! never shifts the boundary) and avoids the deep-offset scan + parallel
//! `count(*)` an offset envelope traditionally pays for.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::AppError;
use crate::error::codes;

/// Cursor for a history list ordered by `(started_at DESC, id DESC)`:
/// incidents, maintenance windows, or both merged. The id is the tiebreaker
/// so two rows sharing a `started_at` always sort deterministically;
/// otherwise a page boundary at the tie could lose or duplicate rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HistoryCursor {
    pub started_at: DateTime<Utc>,
    pub id: Uuid,
}

impl HistoryCursor {
    pub fn encode(&self) -> String {
        encode(self)
    }

    /// Decode an operator- or user-supplied cursor string. Surface a 400
    /// with a stable `INVALID_CURSOR` code rather than 500; a bad cursor is
    /// almost always a client bug (truncated URL, manual edit), not a
    /// server failure.
    pub fn decode(raw: &str) -> Result<Self, AppError> {
        decode(raw)
    }
}

/// Cursor for an alert source's delivery log, ordered by
/// `(received_at DESC, dedup_key ASC, event_id ASC)`. Every event of one
/// request shares its `received_at` and may share its key, so the id is what
/// lets a page end anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertEventCursor {
    pub received_at: DateTime<Utc>,
    pub dedup_key: String,
    pub event_id: Uuid,
}

impl AlertEventCursor {
    pub fn encode(&self) -> String {
        encode(self)
    }

    pub fn decode(raw: &str) -> Result<Self, AppError> {
        decode(raw)
    }
}

fn encode<T: Serialize>(cursor: &T) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(cursor).expect("a cursor is JSON-serialisable"))
}

fn decode<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, AppError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| invalid_cursor("cursor is not valid base64-url"))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| invalid_cursor("cursor payload is not the expected schema"))
}

fn invalid_cursor(msg: &'static str) -> AppError {
    AppError::bad_request_field(codes::INVALID_CURSOR, msg, "cursor")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed() -> HistoryCursor {
        HistoryCursor {
            started_at: DateTime::parse_from_rfc3339("2026-05-22T10:15:30.456Z")
                .unwrap()
                .with_timezone(&Utc),
            id: Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap(),
        }
    }

    #[test]
    fn round_trips() {
        let c = fixed();
        let s = c.encode();
        let back = HistoryCursor::decode(&s).expect("round-trip must succeed");
        assert_eq!(back, c);
    }

    #[test]
    fn encoded_form_is_url_safe() {
        let c = fixed();
        let s = c.encode();
        // No padding, no '+', no '/'. Safe to splice into a query string
        // without further escaping.
        assert!(!s.contains('='));
        assert!(!s.contains('+'));
        assert!(!s.contains('/'));
    }

    #[test]
    fn rejects_garbage() {
        let err = HistoryCursor::decode("not a real cursor !!").expect_err("garbage must reject");
        match err {
            AppError::BadRequest { code, field, .. } => {
                assert_eq!(code, codes::INVALID_CURSOR);
                assert_eq!(field.as_deref(), Some("cursor"));
            }
            other => panic!("expected BadRequest(INVALID_CURSOR), got {other:?}"),
        }
    }

    #[test]
    fn an_alert_event_cursor_without_its_event_id_is_refused() {
        let before = URL_SAFE_NO_PAD
            .encode(br#"{"received_at":"2026-05-22T10:15:30.456Z","dedup_key":"checkout-5xx"}"#);
        let err = AlertEventCursor::decode(&before).expect_err("an older cursor must reject");
        assert!(matches!(
            err,
            AppError::BadRequest { code, .. } if code == codes::INVALID_CURSOR
        ));
    }

    #[test]
    fn rejects_wrong_schema() {
        let bogus = URL_SAFE_NO_PAD.encode(b"{\"not\":\"a cursor\"}");
        let err = HistoryCursor::decode(&bogus).expect_err("wrong schema must reject");
        assert!(matches!(
            err,
            AppError::BadRequest { code, .. } if code == codes::INVALID_CURSOR
        ));
    }
}
