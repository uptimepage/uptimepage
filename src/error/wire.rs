use serde::Serialize;
use utoipa::ToSchema;

/// Top-level envelope returned for every 4xx/5xx response.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApiError {
    pub error: ApiErrorBody,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ApiErrorBody {
    /// Stable machine-readable code (UPPER_SNAKE_CASE).
    #[schema(example = "INVALID_URL_SCHEME")]
    pub code: &'static str,
    /// Human-readable, safe to display.
    #[schema(example = "URL scheme must be http or https.")]
    pub message: String,
    /// JSON pointer to the offending field for 400s.
    #[schema(example = "check.url", nullable = true)]
    pub field: Option<String>,
    /// Optional structured context.
    #[schema(nullable = true)]
    pub details: Option<serde_json::Value>,
    /// W3C traceparent for support.
    #[schema(example = "00-7c3a4f...-01", nullable = true)]
    pub trace_id: Option<String>,
}

impl ApiErrorBody {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            field: None,
            details: None,
            trace_id: None,
        }
    }

    pub fn with_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }
}
