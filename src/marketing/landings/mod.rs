//! SEO landing pages — use-case and comparison pages on the marketing
//! host (`/status-page-for-saas`, `/vs/uptimerobot`, …).
//!
//! Copy is authored factual about Uptimepage. Most comparison pages state
//! what Uptimepage offers and stay neutral about the named competitor, with
//! no third-party feature claims to keep current. The exception is the
//! `matrices` tables. Vet any competitor-specific copy before adding it.

mod catalog;
mod faqs;
mod matrices;
mod model;
mod render;
mod sections;
#[cfg(test)]
mod tests;

pub use catalog::LANDINGS;
#[cfg(test)]
pub(crate) use faqs::page_faqs;
pub use model::{CodeSample, Feature, Landing, ResourceLink, Section};
pub use render::mount;

pub(super) use matrices::page_matrix;
pub(crate) use render::warm;
pub(super) use sections::{page_callout, page_fit};
