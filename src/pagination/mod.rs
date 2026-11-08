//! Page envelopes and opaque cursors shared by every list surface: the
//! API, the console and the public status page.

pub mod cursor;
pub mod page;

pub use cursor::{AlertEventCursor, HistoryCursor};
pub use page::{
    CursorPage, CursorPageOfPublicIncident, CursorPageOfPublicMaintenance, PageEnvelope,
    PageOfCheckResult, PageOfIncident, PageOfMaintenanceWindow, PageOfPublicIncident,
    PageOfTagCount, PageOfTarget,
};
