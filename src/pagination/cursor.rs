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

/// Cursor for an incident list ordered by `(started_at DESC, id DESC)`. The
/// id is the tiebreaker so two incidents sharing a `started_at` always sort
/// deterministically; otherwise a page boundary at the tie could lose or
/// duplicate rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncidentCursor {
    pub started_at: DateTime<Utc>,
    pub id: Uuid,
}

impl IncidentCursor {
    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("IncidentCursor JSON-serialisable");
        URL_SAFE_NO_PAD.encode(json)
    }

    /// Decode an operator- or user-supplied cursor string. Surface a 400
    /// with a stable `INVALID_CURSOR` code rather than 500; a bad cursor is
    /// almost always a client bug (truncated URL, manual edit), not a
    /// server failure.
    pub fn decode(raw: &str) -> Result<Self, AppError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(raw)
            .map_err(|_| invalid_cursor("cursor is not valid base64-url"))?;
        serde_json::from_slice(&bytes)
            .map_err(|_| invalid_cursor("cursor payload is not the expected schema"))
    }
}

fn invalid_cursor(msg: &'static str) -> AppError {
    AppError::bad_request_field(codes::INVALID_CURSOR, msg, "cursor")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed() -> IncidentCursor {
        IncidentCursor {
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
        let back = IncidentCursor::decode(&s).expect("round-trip must succeed");
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
        let err = IncidentCursor::decode("not a real cursor !!").expect_err("garbage must reject");
        match err {
            AppError::BadRequest { code, field, .. } => {
                assert_eq!(code, codes::INVALID_CURSOR);
                assert_eq!(field.as_deref(), Some("cursor"));
            }
            other => panic!("expected BadRequest(INVALID_CURSOR), got {other:?}"),
        }
    }

    #[test]
    fn rejects_wrong_schema() {
        let bogus = URL_SAFE_NO_PAD.encode(b"{\"not\":\"a cursor\"}");
        let err = IncidentCursor::decode(&bogus).expect_err("wrong schema must reject");
        assert!(matches!(
            err,
            AppError::BadRequest { code, .. } if code == codes::INVALID_CURSOR
        ));
    }
}
