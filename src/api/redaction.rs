use axum::Json;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::security::redaction::RedactInPlace;

/// Response wrapper that redacts credential fields before serialization. The
/// inner value is private so the only path from a `Target` (or `Vec<Target>`)
/// to JSON in a handler runs through `IntoResponse`, enforcing redaction at
/// the type level.
pub struct Redacted<T>(T);

impl<T> Redacted<T> {
    pub fn new(inner: T) -> Self {
        Self(inner)
    }
}

impl<T> IntoResponse for Redacted<T>
where
    T: RedactInPlace + Serialize,
{
    fn into_response(self) -> Response {
        let Self(mut inner) = self;
        inner.redact_in_place();
        Json(inner).into_response()
    }
}
