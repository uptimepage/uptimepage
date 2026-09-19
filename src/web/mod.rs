//! Server-rendered UI: askama views, their routes, filters and static assets.
//! It owns no domain logic and no mutation routes — every UI mutation hits an
//! existing API endpoint directly. Request plumbing lives in `crate::request`.

pub mod assets;
pub mod avatar;
pub mod error;
pub mod filters;
pub mod robots;
pub mod routes;
pub mod views;

pub use routes::routes;
