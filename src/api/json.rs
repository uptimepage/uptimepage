//! `axum::Json` whose rejection is the API error envelope, so a body that
//! fails to decode answers with a code like every other refusal. `Json`
//! also refuses a key no object in the body's schema declares, which is how
//! the stored shapes nested in a body stay strict at the boundary without
//! carrying `deny_unknown_fields` themselves. `LenientJson` keeps only the
//! envelope, for bodies an agent on another build sends.

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Request};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;
use utoipa::PartialSchema;

use super::error::codes;
use super::strict;
use crate::error::AppError;

#[derive(Debug, Clone, Copy, Default)]
pub struct Json<T>(pub T);

impl<T, S> FromRequest<S> for Json<T>
where
    T: DeserializeOwned + PartialSchema + 'static,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        if !json_content_type(req.headers()) {
            return Err(AppError::unsupported_media_type(
                codes::INVALID_CONTENT_TYPE,
                "expected request with `Content-Type: application/json`",
            ));
        }
        let bytes = Bytes::from_request(req, state)
            .await
            .map_err(|e| match e.status() {
                axum::http::StatusCode::PAYLOAD_TOO_LARGE => {
                    AppError::payload_too_large(codes::INVALID_JSON, e.body_text())
                }
                _ => AppError::bad_request(codes::INVALID_JSON, e.body_text()),
            })?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| {
            AppError::bad_request(
                codes::INVALID_JSON,
                format!("Failed to parse the request body as JSON: {e}"),
            )
        })?;
        if let Some(message) = strict::describe(&value, &schema_of::<T>()) {
            return Err(AppError::unprocessable(codes::INVALID_JSON, message));
        }
        // From the bytes, not the value: a duplicate key is refused here and
        // would have collapsed silently in the value.
        serde_json::from_slice(&bytes).map(Self).map_err(|e| {
            AppError::unprocessable(
                codes::INVALID_JSON,
                format!("Failed to deserialize the JSON body into the target type: {e}"),
            )
        })
    }
}

fn schema_of<T: PartialSchema + 'static>() -> Arc<serde_json::Value> {
    static SCHEMAS: LazyLock<Mutex<HashMap<TypeId, Arc<serde_json::Value>>>> =
        LazyLock::new(Mutex::default);
    let mut cache = SCHEMAS.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .entry(TypeId::of::<T>())
        .or_insert_with(|| Arc::new(serde_json::to_value(T::schema()).unwrap_or_default()))
        .clone()
}

fn json_content_type(headers: &HeaderMap) -> bool {
    let Some(content_type) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let media = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    media == "application/json" || (media.starts_with("application/") && media.ends_with("+json"))
}

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LenientJson<T>(pub T);

impl<T, S> FromRequest<S> for LenientJson<T>
where
    axum::Json<T>: FromRequest<S, Rejection = JsonRejection>,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match axum::Json::<T>::from_request(req, state).await {
            Ok(axum::Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(rejection.into()),
        }
    }
}

impl<T: Serialize> IntoResponse for LenientJson<T> {
    fn into_response(self) -> Response {
        Json(self.0).into_response()
    }
}

impl From<JsonRejection> for AppError {
    fn from(rejection: JsonRejection) -> Self {
        let message = rejection.body_text();
        match rejection {
            JsonRejection::JsonDataError(_) => Self::unprocessable(codes::INVALID_JSON, message),
            JsonRejection::MissingJsonContentType(_) => {
                Self::unsupported_media_type(codes::INVALID_CONTENT_TYPE, message)
            }
            other if other.status() == axum::http::StatusCode::PAYLOAD_TOO_LARGE => {
                Self::payload_too_large(codes::INVALID_JSON, message)
            }
            _ => Self::bad_request(codes::INVALID_JSON, message),
        }
    }
}
