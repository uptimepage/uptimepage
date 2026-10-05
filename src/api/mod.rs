pub mod docs;
pub mod handlers;
pub mod idempotency;
pub mod json_arc;
pub mod middleware;
pub mod redaction;
pub mod routes;
pub mod types;

pub use docs::ApiDoc;
pub use json_arc::JsonArc;
pub use routes::build_router;
pub use types::{
    BulkAction, BulkActionFailure, BulkActionRequest, BulkActionResponse, DashboardSummary,
    Last24hSummary, StatusBreakdown, SystemSummary, TestRequest, TestResponse,
};
