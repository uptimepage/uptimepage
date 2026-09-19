//! What each account agreed to when it was created. Signup binds the current
//! value at INSERT — no SQL default, which would freeze on the v1-era.
//!
//! Nothing reads these back. Bumping one changes what new accounts record and
//! nothing else: existing rows keep their old value with no way to move them,
//! and no one is re-prompted, because there is no re-accept panel. Treat a bump
//! as marking a boundary in the signup record, never as evidence that anybody
//! saw the new document — the policy page's own "Last updated" line is what a
//! data-subject request reads.

pub const TERMS_VERSION: &str = "v1";
pub const PRIVACY_VERSION: &str = "v1";
