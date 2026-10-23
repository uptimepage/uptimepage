//! What each account agreed to when it was created. Signup binds the
//! configured value at INSERT — no SQL default, which would freeze on the
//! v1-era.
//!
//! Nothing reads these back. Bumping one changes what new accounts record and
//! nothing else: existing rows keep their old value with no way to move them,
//! and no one is re-prompted, because there is no re-accept panel. Treat a bump
//! as marking a boundary in the signup record, never as evidence that anybody
//! saw the new document — the policy page's own "Last updated" line is what a
//! data-subject request reads.

/// The policy versions on the page that creates the account. `None` where the
/// deployment publishes no such policy, so nothing is recorded as accepted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Consent<'a> {
    pub terms: Option<&'a str>,
    pub privacy: Option<&'a str>,
}
