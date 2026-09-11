//! `axum::Json` whose rejection is the API error envelope, so a body that
//! fails to decode answers with a code like every other refusal.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Request};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use super::error::codes;
use crate::error::AppError;

#[derive(Debug, Clone, Copy, Default)]
pub struct Json<T>(pub T);

impl<T, S> FromRequest<S> for Json<T>
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

impl<T: Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
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
